//! The public dashboard API (Laysi/trustiot-v2#150, `assistant::api`): served in the test
//! process with the stand-in token exchange and telemetry-query of `dashboard_world`, on the
//! product's schema, and once as the binary the chart starts.
//!
//! - Requests are strict (unknown fields and parameters refused, no field for SQL, a semantic
//!   request or a page) and every answer is `cache-control: no-store`.
//! - The role comes from the request's token: a viewer cannot publish, restore a saved
//!   dashboard or archive; a draft is its conversation's owner's, and anyone else in the tenant
//!   or outside it gets the same 404 as for a dashboard that does not exist.
//! - Versions: each write adds one, old ones stay readable, a restore adds a version that points
//!   at the content it restores, and publishing over a saved dashboard that changed is a 409,
//!   judged by the base of the version published, which a restore keeps.
//! - Dataset runs act as the viewer: the token telemetry-query gets is the viewer's, with the
//!   viewer's tenant and role. More rows than the dataset allows is refused with the limit; the
//!   caps refuse the 61st run in a minute and a third at a time per tenant while other tenants
//!   run, and the `dashboard` tool's runs count against the same caps; a run waiting for
//!   telemetry-query holds no memory, an answer holds its result's memory until its body is
//!   handed to the connection or dropped (or its bound passed: a reader that stops loses the
//!   answer and its connection), and one tenant holds at most its share; telemetry-query's refusals reach
//!   the user only as closed keys, never as its text.
//! - Opening a dashboard and running all its datasets calls no model.
//!
//! Starts a throwaway Postgres with TLS (podman or docker); skipped without either unless `CI`
//! is set.

mod dashboard_world;

use std::collections::HashSet;
use std::io::{self, IoSlice};
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use assistant::callee::{Caller, Operation};
use assistant::dashboards::caps::{CONCURRENT_RUNS, Clock, RUNS_PER_MINUTE, RunCaps};
use assistant::dashboards::memory::ResultMemory;
use assistant::dashboards::runner::DatasetRunner;
use assistant::model::BoxFuture as ModelFuture;
use assistant::tools::{Delegation, DelegationError};
use axum::http::Method;
use axum::http::header::CONTENT_TYPE;
use contract::api::dashboard::result::DatasetResult;
use contract::api::dashboard::spec::Dataset;
use contract::{
    Actor, DashboardId, INTERNAL_IDENTITY_HEADER, InternalClaims, InternalToken, ServiceName,
    TenantGrant, TenantId, TenantRole,
};
use dashboard_world::{
    Answer, Binary, Content, DATASETS, Peers, Person, SQL_PATH, STATISTICS_PATH, Script,
    TelemetryCall, USAGE_PATH, World, call, call_raw, spec, telemetry_client, wide_sql_answer,
    world,
};
use serde_json::{Value, json};
use testkit::identity::TestIssuer;
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _, ReadBuf};
use tokio::net::{TcpListener, TcpSocket, TcpStream};
use tokio::sync::{Semaphore, mpsc};
use tower::ServiceExt as _;
use uuid::Uuid;

const DASHBOARDS: &str = "/api/assistant/v1/dashboards";

fn dashboard(id: DashboardId) -> String {
    format!("{DASHBOARDS}/{id}")
}

fn action(id: DashboardId, action: &str) -> String {
    format!("{DASHBOARDS}/{id}/{action}")
}

fn drafts(conversation: Uuid) -> String {
    format!("/api/assistant/v1/conversations/{conversation}/dashboards")
}

fn run_body(version: u32, dataset: &str) -> Value {
    json!({"version": version, "dataset": dataset})
}

/// Every answer of the API says it must not be stored.
fn assert_no_store(answer: &Answer) {
    assert_eq!(
        answer.header("cache-control"),
        Some("no-store"),
        "{answer:?}"
    );
}

/// An M1 error with `status`, `code` and `key`, not stored.
fn assert_error(answer: &Answer, status: u16, code: &str, key: &str) {
    assert_eq!(answer.status, status, "{answer:?}");
    assert_eq!(
        answer.error(),
        (code.to_owned(), key.to_owned()),
        "{answer:?}"
    );
    assert_no_store(answer);
}

fn assert_not_found(answer: &Answer) {
    assert_error(answer, 404, "not_found", "error.not_found");
}

/// The world with the stand-in peers and the API in this process.
struct Setup {
    world: World,
    peers: Peers,
    api: dashboard_world::Api,
}

async fn setup(caps: RunCaps) -> Option<Setup> {
    let world = world().await?;
    let peers = Peers::start(Arc::clone(&world.issuer)).await;
    let api = world.api(&peers, caps).await;
    Some(Setup { world, peers, api })
}

/// A clock that moves only when the test says so (milliseconds since it was made).
fn manual_clock() -> (Clock, Arc<AtomicU64>) {
    let start = Instant::now();
    let elapsed_ms = Arc::new(AtomicU64::new(0));
    let shared = Arc::clone(&elapsed_ms);
    let clock: Clock =
        Arc::new(move || start + Duration::from_millis(shared.load(Ordering::SeqCst)));
    (clock, elapsed_ms)
}

/// Every version of a dashboard, as the database holds them.
async fn version_count(world: &World, dashboard: DashboardId) -> i64 {
    let mut admin = world.superuser().await;
    sqlx::query_scalar(
        "SELECT count(*) FROM assistant.dashboard_versions WHERE dashboard_id = $1::uuid",
    )
    .bind(dashboard.to_string())
    .fetch_one(&mut admin)
    .await
    .expect("count the versions")
}

#[tokio::test(flavor = "multi_thread")]
async fn requests_are_strict_and_no_answer_is_stored() {
    let Some(Setup { world, peers, api }) = setup(RunCaps::default()).await else {
        return;
    };
    let base = api.url.as_str();
    let editor = world.person(TenantId::generate(), TenantRole::Editor);
    let (conversation, draft, saved) = world.saved(&editor, &Content::new("Strict")).await;
    let token = world.token(&editor);

    // Every route: 401 without a token, 403 for a user without a tenant.
    let routes: Vec<(Method, String, Option<Value>)> = vec![
        (Method::GET, DASHBOARDS.to_owned(), None),
        (Method::GET, drafts(conversation), None),
        (Method::GET, dashboard(saved), None),
        (Method::GET, action(saved, "versions"), None),
        (
            Method::POST,
            action(draft, "publish"),
            Some(json!({"version": 1})),
        ),
        (
            Method::POST,
            action(saved, "restore"),
            Some(json!({"from_version": 1, "base_version": 1})),
        ),
        (Method::PUT, action(saved, "archive"), None),
        (Method::DELETE, action(saved, "archive"), None),
        (
            Method::POST,
            action(saved, "dataset-runs"),
            Some(run_body(1, "daily")),
        ),
    ];
    let tenantless = world.token(&world.tenantless());
    for (method, path, body) in &routes {
        let anonymous = call(base, method.clone(), path, None, body.as_ref()).await;
        assert_error(&anonymous, 401, "unauthenticated", "error.unauthenticated");
        let forged = call(
            base,
            method.clone(),
            path,
            Some("not.a.token"),
            body.as_ref(),
        )
        .await;
        assert_error(&forged, 401, "unauthenticated", "error.unauthenticated");
        let without = call(base, method.clone(), path, Some(&tenantless), body.as_ref()).await;
        assert_error(
            &without,
            403,
            "permission_denied",
            "error.permission_denied",
        );
    }

    // An ID that is not a UUID is a dashboard that does not exist.
    for path in [
        format!("{DASHBOARDS}/not-a-uuid"),
        format!("{DASHBOARDS}/not-a-uuid/versions"),
        "/api/assistant/v1/conversations/1/dashboards".to_owned(),
    ] {
        assert_not_found(&call(base, Method::GET, &path, Some(&token), None).await);
    }

    // Query strings: unknown parameters, bad values and cursors this service did not write.
    for path in [
        format!("{DASHBOARDS}?unknown=1"),
        format!("{DASHBOARDS}?include_archived=maybe"),
        format!("{DASHBOARDS}?include_archived=true&include_archived=false"),
        format!("{DASHBOARDS}?cursor=abc"),
        format!("{}?cursor=abc", drafts(conversation)),
        format!("{}?include_archived=true", drafts(conversation)),
        format!("{}?version=x", dashboard(saved)),
        format!("{}?version=-1", dashboard(saved)),
        format!("{}?version=1&page=1", dashboard(saved)),
        format!("{}?cursor=0", action(saved, "versions")),
        format!("{}?cursor=abc", action(saved, "versions")),
    ] {
        let answer = call(base, Method::GET, &path, Some(&token), None).await;
        assert_error(&answer, 400, "invalid_request", "error.invalid_request");
    }

    // Bodies: no field beyond the request's, none for SQL, a semantic request or a page.
    let sql = json!("SELECT * FROM trustiot.tenant_telemetry");
    let refused: Vec<(String, Value)> = vec![
        (
            action(draft, "publish"),
            json!({"version": 1, "spec": spec()}),
        ),
        (
            action(draft, "publish"),
            json!({"version": 1, "page": "<p>x</p>"}),
        ),
        (action(draft, "publish"), json!({"version": "1"})),
        (action(draft, "publish"), json!({})),
        (
            action(saved, "restore"),
            json!({"from_version": 1, "base_version": 1, "title": "x"}),
        ),
        (action(saved, "restore"), json!({"from_version": 1})),
        (
            action(saved, "dataset-runs"),
            json!({"version": 1, "dataset": "daily", "sql": sql}),
        ),
        (
            action(saved, "dataset-runs"),
            json!({"version": 1, "dataset": "daily", "source": {"kind": "sql", "sql": sql}}),
        ),
        (
            action(saved, "dataset-runs"),
            json!({"version": 1, "dataset": "stats", "request": {"tz": "UTC"}}),
        ),
        (
            action(saved, "dataset-runs"),
            json!({"version": 1, "dataset": "x".repeat(5000)}),
        ),
        // serde reads a struct from an array of its fields too; only the object is the request.
        (action(saved, "dataset-runs"), json!([1, "daily"])),
        (action(draft, "publish"), json!([1])),
        (action(saved, "dataset-runs"), Value::Null),
        (action(saved, "dataset-runs"), json!("daily")),
    ];
    for (path, body) in &refused {
        let answer = call(base, Method::POST, path, Some(&token), Some(body)).await;
        assert_error(&answer, 400, "invalid_request", "error.invalid_request");
    }
    // Only JSON: another content type, none, two, or text that is not JSON.
    let body = br#"{"version": 1, "dataset": "daily"}"#;
    let path = action(saved, "dataset-runs");
    for content_type in [
        "text/plain",
        "application/x-www-form-urlencoded",
        "application/jsonx",
    ] {
        let answer = call_raw(
            base,
            Method::POST,
            &path,
            Some(&token),
            Some((content_type, body)),
        )
        .await;
        assert_error(&answer, 400, "invalid_request", "error.invalid_request");
    }
    let answer = call_raw(base, Method::POST, &path, Some(&token), None).await;
    assert_error(&answer, 400, "invalid_request", "error.invalid_request");
    // Text that is not one JSON object written once: truncated, a field twice (the last would
    // win in a map), trailing text, and an array after whitespace.
    for text in [
        &b"{\"version\": 1,"[..],
        br#"{"version": 2, "dataset": "daily", "version": 1}"#,
        br#"{"version": 1, "dataset": "stats", "dataset": "daily"}"#,
        br#"{"version": 1, "dataset": "daily"} {}"#,
        b" \n\t[1, \"daily\"]",
    ] {
        let answer = call_raw(
            base,
            Method::POST,
            &path,
            Some(&token),
            Some(("application/json", text)),
        )
        .await;
        assert_error(&answer, 400, "invalid_request", "error.invalid_request");
    }
    // JSON's whitespace before the object is still the object.
    let answer = call_raw(
        base,
        Method::POST,
        &path,
        Some(&token),
        Some((
            "application/json",
            b" \r\n\t{\"version\": 1, \"dataset\": \"daily\"}",
        )),
    )
    .await;
    assert_eq!(
        answer.status, 200,
        "whitespace before the object: {answer:?}"
    );
    let answer = call_raw(
        base,
        Method::POST,
        &path,
        Some(&token),
        Some(("Application/JSON; charset=utf-8", body)),
    )
    .await;
    assert_eq!(
        answer.status, 200,
        "the content type is compared without case: {answer:?}"
    );
    assert_no_store(&answer);

    // Nothing above wrote a version, and only the two accepted runs reached telemetry-query.
    assert_eq!(version_count(&world, saved).await, 1);
    assert_eq!(version_count(&world, draft).await, 1);
    assert_eq!(peers.calls().len(), 2);

    // The answers that succeed are not stored either.
    for path in [
        DASHBOARDS.to_owned(),
        drafts(conversation),
        dashboard(saved),
        action(saved, "versions"),
    ] {
        let answer = call(base, Method::GET, &path, Some(&token), None).await;
        assert_eq!(answer.status, 200, "{answer:?}");
        assert_no_store(&answer);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_requests_role_decides_who_publishes_restores_and_archives() {
    let Some(Setup { world, api, .. }) = setup(RunCaps::default()).await else {
        return;
    };
    let base = api.url.as_str();
    let tenant = TenantId::generate();
    let editor = world.person(tenant, TenantRole::Editor);
    let viewer = world.person(tenant, TenantRole::Viewer);
    let (_, _, saved) = world.saved(&editor, &Content::new("Saved")).await;
    let conversation = world.conversation(&viewer).await;
    let draft = world
        .create_draft(&viewer, conversation, &Content::new("Viewer's draft"))
        .await
        .dashboard_id;
    let as_viewer = world.token(&viewer);

    // What the view says each may do.
    let permissions = |answer: &Answer| answer.json()["permissions"].clone();
    let seen = call(base, Method::GET, &dashboard(saved), Some(&as_viewer), None).await;
    assert_eq!(
        permissions(&seen),
        json!({"can_publish": false, "can_restore": false, "can_archive": false})
    );
    let seen = call(
        base,
        Method::GET,
        &dashboard(saved),
        Some(&world.token(&editor)),
        None,
    )
    .await;
    assert_eq!(
        permissions(&seen),
        json!({"can_publish": false, "can_restore": true, "can_archive": true})
    );
    let seen = call(base, Method::GET, &dashboard(draft), Some(&as_viewer), None).await;
    assert_eq!(
        permissions(&seen),
        json!({"can_publish": false, "can_restore": true, "can_archive": false})
    );

    // A viewer publishes nothing, and restores and archives no saved dashboard.
    let refusals = [
        (
            Method::POST,
            action(draft, "publish"),
            Some(json!({"version": 1})),
        ),
        (
            Method::POST,
            action(saved, "restore"),
            Some(json!({"from_version": 1, "base_version": 1})),
        ),
        (Method::PUT, action(saved, "archive"), None),
        (Method::DELETE, action(saved, "archive"), None),
    ];
    for (method, path, body) in &refusals {
        let answer = call(base, method.clone(), path, Some(&as_viewer), body.as_ref()).await;
        assert_error(
            &answer,
            403,
            "permission_denied",
            "dashboard.editor_required",
        );
    }
    assert_eq!(version_count(&world, saved).await, 1);
    assert_eq!(version_count(&world, draft).await, 1);

    // A viewer restores their own draft.
    world
        .write_draft(
            &viewer,
            conversation,
            draft,
            1,
            None,
            &Content::new("Viewer's second"),
        )
        .await
        .expect("version 2");
    let restored = call(
        base,
        Method::POST,
        &action(draft, "restore"),
        Some(&as_viewer),
        Some(&json!({"from_version": 1, "base_version": 2})),
    )
    .await;
    assert_eq!(restored.status, 201, "{restored:?}");
    assert_eq!(
        restored.json(),
        json!({"dashboard_id": draft, "version": 3})
    );

    // The role is the token's, on every request: the same user made an editor publishes.
    let promoted = viewer.with_role(TenantRole::Editor);
    let seen = call(
        base,
        Method::GET,
        &dashboard(draft),
        Some(&world.token(&promoted)),
        None,
    )
    .await;
    assert_eq!(
        permissions(&seen),
        json!({"can_publish": true, "can_restore": true, "can_archive": false})
    );
    let published = call(
        base,
        Method::POST,
        &action(draft, "publish"),
        Some(&world.token(&promoted)),
        Some(&json!({"version": 3})),
    )
    .await;
    assert_eq!(published.status, 201, "{published:?}");
    assert_eq!(published.json()["version"], 1);
    // Demoted again, the same user is refused again.
    let answer = call(
        base,
        Method::POST,
        &action(draft, "publish"),
        Some(&as_viewer),
        Some(&json!({"version": 3})),
    )
    .await;
    assert_error(
        &answer,
        403,
        "permission_denied",
        "dashboard.editor_required",
    );

    // The kinds: a saved dashboard is not published, a draft is not archived.
    let as_editor = world.token(&editor);
    let answer = call(
        base,
        Method::POST,
        &action(saved, "publish"),
        Some(&as_editor),
        Some(&json!({"version": 1})),
    )
    .await;
    assert_error(
        &answer,
        422,
        "validation_failed",
        "dashboard.draft_required",
    );
    let editor_conversation = world.conversation(&editor).await;
    let editor_draft = world
        .create_draft(
            &editor,
            editor_conversation,
            &Content::new("Editor's draft"),
        )
        .await
        .dashboard_id;
    let answer = call(
        base,
        Method::PUT,
        &action(editor_draft, "archive"),
        Some(&as_editor),
        None,
    )
    .await;
    assert_error(
        &answer,
        422,
        "validation_failed",
        "dashboard.saved_required",
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn someone_elses_draft_is_not_found_inside_the_tenant_or_outside_it() {
    let Some(Setup { world, peers, api }) = setup(RunCaps::default()).await else {
        return;
    };
    let base = api.url.as_str();
    let tenant = TenantId::generate();
    let owner = world.person(tenant, TenantRole::Editor);
    let conversation = world.conversation(&owner).await;
    let draft = world
        .create_draft(&owner, conversation, &Content::new("Owner's draft"))
        .await
        .dashboard_id;
    // The owner sees the draft and the conversation's list.
    let mine = world.token(&owner);
    let listed = call(base, Method::GET, &drafts(conversation), Some(&mine), None).await;
    assert_eq!(listed.status, 200, "{listed:?}");
    assert_eq!(listed.json()["dashboards"][0]["dashboard_id"], json!(draft));

    // An admin of the same tenant, and an admin of another tenant: the same 404 as for a
    // dashboard or conversation that does not exist, for every route, and nothing is written
    // or run.
    let colleague = world.person(tenant, TenantRole::Admin);
    let stranger = world.person(TenantId::generate(), TenantRole::Admin);
    for other in [&colleague, &stranger] {
        let token = world.token(other);
        let requests: Vec<(Method, String, Option<Value>)> = vec![
            (Method::GET, drafts(conversation), None),
            (Method::GET, dashboard(draft), None),
            (Method::GET, action(draft, "versions"), None),
            (
                Method::POST,
                action(draft, "publish"),
                Some(json!({"version": 1})),
            ),
            (
                Method::POST,
                action(draft, "restore"),
                Some(json!({"from_version": 1, "base_version": 1})),
            ),
            (Method::PUT, action(draft, "archive"), None),
            (
                Method::POST,
                action(draft, "dataset-runs"),
                Some(run_body(1, "daily")),
            ),
        ];
        for (method, path, body) in &requests {
            let answer = call(base, method.clone(), path, Some(&token), body.as_ref()).await;
            assert_not_found(&answer);
            assert!(!answer.text.contains(&draft.to_string()), "{answer:?}");
        }
        // As for what does not exist.
        let nothing = call(
            base,
            Method::GET,
            &dashboard(DashboardId::generate()),
            Some(&token),
            None,
        )
        .await;
        assert_eq!(
            nothing.text,
            call(base, Method::GET, &dashboard(draft), Some(&token), None)
                .await
                .text
        );
        let none = call(
            base,
            Method::GET,
            &drafts(Uuid::now_v7()),
            Some(&token),
            None,
        )
        .await;
        assert_not_found(&none);
    }
    assert_eq!(version_count(&world, draft).await, 1);
    assert!(
        peers.calls().is_empty(),
        "no dataset ran: {:?}",
        peers.calls()
    );
    let mut admin = world.superuser().await;
    let saved: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM assistant.dashboards WHERE tenant_id = ANY($1::uuid[]) AND kind = 'saved'",
    )
    .bind(vec![tenant.to_string(), stranger.tenant().to_string()])
    .fetch_one(&mut admin)
    .await
    .expect("count");
    assert_eq!(saved, 0, "nothing was published");
}

#[tokio::test(flavor = "multi_thread")]
async fn versions_are_added_kept_restored_and_publishing_over_a_change_conflicts() {
    let Some(Setup { world, api, .. }) = setup(RunCaps::default()).await else {
        return;
    };
    let base = api.url.as_str();
    let tenant = TenantId::generate();
    let alice = world.person(tenant, TenantRole::Editor);
    let bob = world.person(tenant, TenantRole::Editor);
    let as_alice = world.token(&alice);
    let as_bob = world.token(&bob);
    let conversation = world.conversation(&alice).await;
    let draft = world
        .create_draft(&alice, conversation, &Content::new("First"))
        .await
        .dashboard_id;
    world
        .write_draft(
            &alice,
            conversation,
            draft,
            1,
            None,
            &Content::new("Second"),
        )
        .await
        .expect("version 2");

    // Each write added a version; the old one is still there as it was.
    let latest = call(base, Method::GET, &dashboard(draft), Some(&as_alice), None).await;
    assert_eq!(latest.json()["version"]["title"], "Second");
    assert_eq!(latest.json()["dashboard"]["latest_version"], 2);
    let old = call(
        base,
        Method::GET,
        &format!("{}?version=1", dashboard(draft)),
        Some(&as_alice),
        None,
    )
    .await;
    assert_eq!(old.status, 200, "{old:?}");
    assert_eq!(old.json()["version"]["title"], "First");
    assert_eq!(old.json()["version"]["page"], Content::new("First").page);
    let versions = call(
        base,
        Method::GET,
        &action(draft, "versions"),
        Some(&as_alice),
        None,
    )
    .await;
    let numbers: Vec<Value> = versions.json()["versions"]
        .as_array()
        .expect("versions")
        .iter()
        .map(|version| version["version"].clone())
        .collect();
    assert_eq!(numbers, [json!(2), json!(1)]);
    let missing = call(
        base,
        Method::GET,
        &format!("{}?version=3", dashboard(draft)),
        Some(&as_alice),
        None,
    )
    .await;
    assert_error(&missing, 404, "not_found", "dashboard.version_not_found");

    // Publishing the version Alice looked at, though the tool wrote a newer one.
    let published = call(
        base,
        Method::POST,
        &action(draft, "publish"),
        Some(&as_alice),
        Some(&json!({"version": 1})),
    )
    .await;
    assert_eq!(published.status, 201, "{published:?}");
    let saved = DashboardId::from_uuid(
        Uuid::try_parse(published.json()["dashboard_id"].as_str().expect("an ID")).expect("a UUID"),
    );
    assert_eq!(published.json()["version"], 1);
    let copy = call(base, Method::GET, &dashboard(saved), Some(&as_bob), None).await;
    assert_eq!(copy.json()["version"]["title"], "First");
    assert_eq!(copy.json()["version"]["origin"], "publish");

    // Alice's version 2 was written before her first publish: it is based on no saved version
    // and is not published over version 1.
    let stale_draft = call(
        base,
        Method::POST,
        &action(draft, "publish"),
        Some(&as_alice),
        Some(&json!({"version": 2})),
    )
    .await;
    assert_error(&stale_draft, 409, "conflict", "dashboard.saved_changed");
    assert_eq!(stale_draft.json()["details"]["latest"], 1);

    // Bob takes a draft of version 1; Alice's tool merges version 1 into her draft and she
    // publishes that as version 2 meanwhile.
    let bob_conversation = world.conversation(&bob).await;
    let bob_draft = world
        .fork(&bob, bob_conversation, saved, 1, &Content::new("Bob's"))
        .await
        .expect("Bob's draft")
        .dashboard_id;
    world
        .write_draft(
            &alice,
            conversation,
            draft,
            2,
            Some(1),
            &Content::new("Merged"),
        )
        .await
        .expect("version 3");
    let second = call(
        base,
        Method::POST,
        &action(draft, "publish"),
        Some(&as_alice),
        Some(&json!({"version": 3})),
    )
    .await;
    assert_eq!(second.json(), json!({"dashboard_id": saved, "version": 2}));
    let conflict = call(
        base,
        Method::POST,
        &action(bob_draft, "publish"),
        Some(&as_bob),
        Some(&json!({"version": 1})),
    )
    .await;
    assert_error(&conflict, 409, "conflict", "dashboard.saved_changed");
    assert_eq!(conflict.json()["details"]["latest"], 2);

    // A restore based on an older version conflicts; based on the latest, it adds a copy.
    let stale = call(
        base,
        Method::POST,
        &action(saved, "restore"),
        Some(&as_bob),
        Some(&json!({"from_version": 1, "base_version": 1})),
    )
    .await;
    assert_error(&stale, 409, "conflict", "dashboard.version_conflict");
    assert_eq!(stale.json()["details"]["latest"], 2);
    let restored = call(
        base,
        Method::POST,
        &action(saved, "restore"),
        Some(&as_bob),
        Some(&json!({"from_version": 1, "base_version": 2})),
    )
    .await;
    assert_eq!(restored.status, 201, "{restored:?}");
    assert_eq!(
        restored.json(),
        json!({"dashboard_id": saved, "version": 3})
    );
    let third = call(
        base,
        Method::GET,
        &format!("{}?version=3", dashboard(saved)),
        Some(&as_alice),
        None,
    )
    .await;
    assert_eq!(third.json()["version"]["origin"], "restore");
    assert_eq!(third.json()["version"]["restored_from"], 1);
    assert_eq!(third.json()["version"]["title"], "First");
    assert_eq!(third.json()["version"]["created_by"], json!(bob.claims.sub));
    let missing = call(
        base,
        Method::POST,
        &action(saved, "restore"),
        Some(&as_bob),
        Some(&json!({"from_version": 9, "base_version": 3})),
    )
    .await;
    assert_error(&missing, 404, "not_found", "dashboard.version_not_found");

    // Archived: out of the list unless asked for, and nothing is published into it.
    let archived = call(
        base,
        Method::PUT,
        &action(saved, "archive"),
        Some(&as_bob),
        None,
    )
    .await;
    assert_eq!(archived.status, 200, "{archived:?}");
    assert!(archived.json()["archived_at"].is_string(), "{archived:?}");
    let listed = |answer: &Answer| -> Vec<Value> {
        answer.json()["dashboards"]
            .as_array()
            .expect("dashboards")
            .iter()
            .map(|summary| summary["dashboard_id"].clone())
            .collect()
    };
    let list = call(base, Method::GET, DASHBOARDS, Some(&as_alice), None).await;
    assert!(listed(&list).is_empty(), "{list:?}");
    let list = call(
        base,
        Method::GET,
        &format!("{DASHBOARDS}?include_archived=true"),
        Some(&as_alice),
        None,
    )
    .await;
    assert_eq!(listed(&list), [json!(saved)]);
    world
        .write_draft(
            &alice,
            conversation,
            draft,
            3,
            None,
            &Content::new("Fourth"),
        )
        .await
        .expect("version 4");
    let refused = call(
        base,
        Method::POST,
        &action(draft, "publish"),
        Some(&as_alice),
        Some(&json!({"version": 4})),
    )
    .await;
    assert_error(&refused, 409, "conflict", "dashboard.archived");
    let unarchived = call(
        base,
        Method::DELETE,
        &action(saved, "archive"),
        Some(&as_alice),
        None,
    )
    .await;
    assert_eq!(unarchived.json(), json!({"dashboard_id": saved}));
    let list = call(base, Method::GET, DASHBOARDS, Some(&as_alice), None).await;
    assert_eq!(listed(&list), [json!(saved)]);
}

/// A restore through the API of `from_version` on `base_version`.
async fn restore(
    base: &str,
    token: &str,
    dashboard: DashboardId,
    from_version: u32,
    base_version: u32,
) -> Answer {
    call(
        base,
        Method::POST,
        &action(dashboard, "restore"),
        Some(token),
        Some(&json!({"from_version": from_version, "base_version": base_version})),
    )
    .await
}

/// A draft of `owner` in a new conversation with two versions of different content.
async fn draft_with_two_versions(world: &World, owner: &Person) -> DashboardId {
    let conversation = world.conversation(owner).await;
    let draft = world
        .create_draft(owner, conversation, &Content::new("First"))
        .await
        .dashboard_id;
    world
        .write_draft(owner, conversation, draft, 1, None, &Content::new("Second"))
        .await
        .expect("version 2");
    draft
}

/// A 429 `dashboard.versions_limited` whose `Retry-After` and `details.retry_after` agree and
/// lie within the minute the budget counts in.
fn assert_versions_limited(answer: &Answer) {
    assert_error(answer, 429, "rate_limited", "dashboard.versions_limited");
    let header: u64 = answer
        .header("retry-after")
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| panic!("a Retry-After in seconds: {answer:?}"));
    assert!((1..=60).contains(&header), "{answer:?}");
    assert_eq!(
        answer.json()["details"]["retry_after"],
        json!(header),
        "{answer:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn versions_added_by_publishing_and_restoring_are_limited_per_user_and_tenant_in_a_minute() {
    let Some(Setup { world, api, .. }) = setup(RunCaps::default()).await else {
        return;
    };
    let base = api.url.as_str();
    let tenant = TenantId::generate();
    let [alice, bob, carol, dave, erin] =
        std::array::from_fn(|_| world.person(tenant, TenantRole::Editor));
    let viewer = world.person(tenant, TenantRole::Viewer);
    let alice_draft = draft_with_two_versions(&world, &alice).await;
    let as_alice = world.token(&alice);
    let publish = |token: &str, draft: DashboardId, version: u32| {
        let token = token.to_owned();
        async move {
            call(
                base,
                Method::POST,
                &action(draft, "publish"),
                Some(&token),
                Some(&json!({"version": version})),
            )
            .await
        }
    };

    // Alice adds 10 versions in the minute: restores of version 1 and 2 in turn.
    for added in 0..10 {
        let restored = restore(base, &as_alice, alice_draft, 1 + added % 2, 2 + added).await;
        assert_eq!(restored.status, 201, "restore {added}: {restored:?}");
    }
    // An 11th version of hers, by restoring or publishing, is refused with when to try again,
    // and nothing is written.
    assert_versions_limited(&restore(base, &as_alice, alice_draft, 1, 12).await);
    assert_versions_limited(&publish(&as_alice, alice_draft, 12).await);
    assert_eq!(version_count(&world, alice_draft).await, 12);

    // A request the store refuses adds no version and counts nothing: Bob's stale restore,
    // then 10 versions of his.
    let bob_draft = draft_with_two_versions(&world, &bob).await;
    let as_bob = world.token(&bob);
    let stale = restore(base, &as_bob, bob_draft, 1, 1).await;
    assert_error(&stale, 409, "conflict", "dashboard.version_conflict");
    for added in 0..10 {
        let restored = restore(base, &as_bob, bob_draft, 1 + added % 2, 2 + added).await;
        assert_eq!(restored.status, 201, "restore {added}: {restored:?}");
    }
    assert_versions_limited(&restore(base, &as_bob, bob_draft, 1, 12).await);

    // Carol's 10 bring the tenant's drafts to 30 versions in the minute: Dave, who has added
    // none, restores no draft now.
    let carol_draft = draft_with_two_versions(&world, &carol).await;
    let as_carol = world.token(&carol);
    for added in 0..10 {
        let restored = restore(base, &as_carol, carol_draft, 1 + added % 2, 2 + added).await;
        assert_eq!(restored.status, 201, "restore {added}: {restored:?}");
    }
    let dave_draft = draft_with_two_versions(&world, &dave).await;
    let as_dave = world.token(&dave);
    assert_versions_limited(&restore(base, &as_dave, dave_draft, 1, 2).await);
    assert_eq!(version_count(&world, dave_draft).await, 2);
    // The tenant's saved dashboards have a budget of their own: Dave publishes.
    let published = publish(&as_dave, dave_draft, 2).await;
    assert_eq!(published.status, 201, "{published:?}");

    // Requests refused for who sent them count nothing, so they cannot use up what others
    // share: a viewer's 40 publishes (more than the tenant's 30) and Erin's 15 publishes of a
    // version that does not exist (more than her 10). Erin then publishes.
    let viewer_draft = draft_with_two_versions(&world, &viewer).await;
    let as_viewer = world.token(&viewer);
    for _ in 0..40 {
        let refused = publish(&as_viewer, viewer_draft, 2).await;
        assert_error(
            &refused,
            403,
            "permission_denied",
            "dashboard.editor_required",
        );
    }
    let erin_draft = draft_with_two_versions(&world, &erin).await;
    let as_erin = world.token(&erin);
    for _ in 0..15 {
        let refused = publish(&as_erin, erin_draft, 99).await;
        assert_error(&refused, 404, "not_found", "dashboard.version_not_found");
    }
    let published = publish(&as_erin, erin_draft, 2).await;
    assert_eq!(published.status, 201, "{published:?}");

    // Another tenant's user is not held back.
    let stranger = world.person(TenantId::generate(), TenantRole::Editor);
    let stranger_draft = draft_with_two_versions(&world, &stranger).await;
    let restored = restore(base, &world.token(&stranger), stranger_draft, 1, 2).await;
    assert_eq!(restored.status, 201, "{restored:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_restore_that_changes_nothing_is_refused_and_a_draft_numbers_its_own_versions() {
    let Some(Setup { world, api, .. }) = setup(RunCaps::default()).await else {
        return;
    };
    let base = api.url.as_str();
    let tenant = TenantId::generate();
    let editor = world.person(tenant, TenantRole::Editor);
    let viewer = world.person(tenant, TenantRole::Viewer);
    let (_, _, saved) = world.saved(&editor, &Content::new("Saved")).await;
    let as_editor = world.token(&editor);
    let as_viewer = world.token(&viewer);

    // Restoring the version that is the latest adds nothing: refused, nothing written.
    let unchanged = restore(base, &as_editor, saved, 1, 1).await;
    assert_error(&unchanged, 409, "conflict", "dashboard.restore_unchanged");
    assert_eq!(version_count(&world, saved).await, 1);

    // A viewer's draft of the saved dashboard has versions of its own: restoring it never adds
    // a version to the saved dashboard.
    let conversation = world.conversation(&viewer).await;
    let draft = world
        .fork(&viewer, conversation, saved, 1, &Content::new("Viewer's"))
        .await
        .expect("the viewer's draft")
        .dashboard_id;
    let unchanged = restore(base, &as_viewer, draft, 1, 1).await;
    assert_error(&unchanged, 409, "conflict", "dashboard.restore_unchanged");
    world
        .write_draft(
            &viewer,
            conversation,
            draft,
            1,
            None,
            &Content::new("Viewer's second"),
        )
        .await
        .expect("version 2");
    for (from, latest) in [(1, 2), (2, 3), (1, 4)] {
        let restored = restore(base, &as_viewer, draft, from, latest).await;
        assert_eq!(
            restored.json(),
            json!({"dashboard_id": draft, "version": latest + 1}),
            "{restored:?}"
        );
    }
    // Version 5 is a copy of version 1: restoring version 1 or 3 again changes nothing either.
    for from in [1, 3, 5] {
        let unchanged = restore(base, &as_viewer, draft, from, 5).await;
        assert_error(&unchanged, 409, "conflict", "dashboard.restore_unchanged");
    }
    assert_eq!(version_count(&world, draft).await, 5);
    assert_eq!(version_count(&world, saved).await, 1);
    let seen = call(base, Method::GET, &dashboard(saved), Some(&as_viewer), None).await;
    assert_eq!(seen.json()["dashboard"]["latest_version"], 1, "{seen:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dashboard_with_the_most_versions_takes_no_more_and_says_why() {
    let Some(Setup { world, api, .. }) = setup(RunCaps::default()).await else {
        return;
    };
    let base = api.url.as_str();
    let tenant = TenantId::generate();
    let editor = world.person(tenant, TenantRole::Editor);
    let (_, _, saved) = world.saved(&editor, &Content::new("Saved")).await;
    // Versions 2 to 10,000, each a copy of version 1 with its own title.
    let mut admin = world.superuser().await;
    sqlx::query(
        "INSERT INTO assistant.dashboard_versions
            (tenant_id, dashboard_id, version, origin, runtime, title, description, spec, page,
             created_by)
        SELECT v.tenant_id, v.dashboard_id, n, 'publish', v.runtime, 'Saved ' || n,
            v.description, v.spec, v.page, v.created_by
        FROM assistant.dashboard_versions v, generate_series(2, 10000) AS n
        WHERE v.dashboard_id = $1::uuid AND v.version = 1",
    )
    .bind(saved.to_string())
    .execute(&mut admin)
    .await
    .expect("fill the saved dashboard's versions");
    drop(admin);
    assert_eq!(version_count(&world, saved).await, 10_000);

    // An editor's restore and the publish of a draft based on the latest are refused with the
    // reason; nothing is written.
    let as_editor = world.token(&editor);
    let refused = restore(base, &as_editor, saved, 1, 10_000).await;
    assert_error(&refused, 409, "conflict", "dashboard.too_many_versions");
    let conversation = world.conversation(&editor).await;
    let draft = world
        .fork(&editor, conversation, saved, 10_000, &Content::new("Next"))
        .await
        .expect("a draft of the latest version")
        .dashboard_id;
    let refused = call(
        base,
        Method::POST,
        &action(draft, "publish"),
        Some(&as_editor),
        Some(&json!({"version": 1})),
    )
    .await;
    assert_error(&refused, 409, "conflict", "dashboard.too_many_versions");
    assert_eq!(version_count(&world, saved).await, 10_000);
}

/// A saved dashboard at version 3 and Bob's draft of it: his version 1 was taken from the
/// saved dashboard's version 1, before Alice published versions 2 and 3, and his version 2
/// merged version 3 in (the tool's rebase). Bob, his conversation, his draft and the saved
/// dashboard.
async fn draft_merged_with_newer_versions(
    world: &World,
) -> (Person, Uuid, DashboardId, DashboardId) {
    let tenant = TenantId::generate();
    let alice = world.person(tenant, TenantRole::Editor);
    let bob = world.person(tenant, TenantRole::Editor);
    let (alice_conversation, alice_draft, saved) =
        world.saved(&alice, &Content::new("Plant")).await;
    let bob_conversation = world.conversation(&bob).await;
    let bob_draft = world
        .fork(
            &bob,
            bob_conversation,
            saved,
            1,
            &Content::new("Bob's plant"),
        )
        .await
        .expect("Bob's draft")
        .dashboard_id;
    for (base, title) in [(1, "Plant 2"), (2, "Plant 3")] {
        world
            .write_draft(
                &alice,
                alice_conversation,
                alice_draft,
                base,
                None,
                &Content::new(title),
            )
            .await
            .expect("Alice's next version");
        world
            .publish(&alice, alice_draft, base + 1)
            .await
            .expect("Alice publishes it");
    }
    world
        .write_draft(
            &bob,
            bob_conversation,
            bob_draft,
            1,
            Some(3),
            &Content::new("Bob's plant with Alice's 3"),
        )
        .await
        .expect("Bob's merged version");
    (bob, bob_conversation, bob_draft, saved)
}

#[tokio::test(flavor = "multi_thread")]
async fn publishing_an_older_draft_version_compares_the_base_it_was_written_on() {
    let Some(Setup { world, api, .. }) = setup(RunCaps::default()).await else {
        return;
    };
    let base = api.url.as_str();
    let (bob, _, draft, saved) = draft_merged_with_newer_versions(&world).await;
    let as_bob = world.token(&bob);
    let publish = |version: u32| {
        let token = as_bob.clone();
        async move {
            call(
                base,
                Method::POST,
                &action(draft, "publish"),
                Some(&token),
                Some(&json!({"version": version})),
            )
            .await
        }
    };

    // Version 1 was written on the saved dashboard's version 1: publishing it would drop
    // Alice's versions 2 and 3, though the draft's later version merged them.
    let refused = publish(1).await;
    assert_error(&refused, 409, "conflict", "dashboard.saved_changed");
    assert_eq!(refused.json()["details"]["latest"], 3);
    assert_eq!(version_count(&world, saved).await, 3);

    // Version 2, merged with version 3, publishes; then neither version is published again
    // over the version 4 it made.
    let published = publish(2).await;
    assert_eq!(
        published.json(),
        json!({"dashboard_id": saved, "version": 4}),
        "{published:?}"
    );
    for version in [1, 2] {
        let refused = publish(version).await;
        assert_error(&refused, 409, "conflict", "dashboard.saved_changed");
        assert_eq!(refused.json()["details"]["latest"], 4);
    }
    assert_eq!(version_count(&world, saved).await, 4);
    // The draft's next version starts from the saved dashboard's version 4.
    let seen = call(base, Method::GET, &dashboard(draft), Some(&as_bob), None).await;
    assert_eq!(
        seen.json()["dashboard"]["saved"],
        json!({"dashboard_id": saved, "version": 4}),
        "{seen:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn restoring_a_draft_version_carries_the_base_it_was_written_on() {
    let Some(Setup { world, api, .. }) = setup(RunCaps::default()).await else {
        return;
    };
    let base = api.url.as_str();
    let (bob, conversation, draft, saved) = draft_merged_with_newer_versions(&world).await;
    let as_bob = world.token(&bob);
    let publish = |version: u32| {
        let token = as_bob.clone();
        async move {
            call(
                base,
                Method::POST,
                &action(draft, "publish"),
                Some(&token),
                Some(&json!({"version": version})),
            )
            .await
        }
    };
    let based_on = || {
        let token = as_bob.clone();
        async move {
            call(base, Method::GET, &dashboard(draft), Some(&token), None)
                .await
                .json()["dashboard"]["saved"]["version"]
                .clone()
        }
    };

    // Restoring version 1 brings back content written on the saved dashboard's version 1:
    // publishing the copy is refused, and so is the tool's next version written on it.
    let restored = restore(base, &as_bob, draft, 1, 2).await;
    assert_eq!(
        restored.json(),
        json!({"dashboard_id": draft, "version": 3}),
        "{restored:?}"
    );
    let refused = publish(3).await;
    assert_error(&refused, 409, "conflict", "dashboard.saved_changed");
    assert_eq!(refused.json()["details"]["latest"], 3);
    assert_eq!(based_on().await, 1);
    world
        .write_draft(
            &bob,
            conversation,
            draft,
            3,
            None,
            &Content::new("Bob's plant, again"),
        )
        .await
        .expect("the tool's version 4");
    let refused = publish(4).await;
    assert_error(&refused, 409, "conflict", "dashboard.saved_changed");
    assert_eq!(version_count(&world, saved).await, 3);

    // Restoring the merged version 2 brings its base back with it: the copy publishes.
    let restored = restore(base, &as_bob, draft, 2, 4).await;
    assert_eq!(
        restored.json(),
        json!({"dashboard_id": draft, "version": 5}),
        "{restored:?}"
    );
    assert_eq!(based_on().await, 3);
    let published = publish(5).await;
    assert_eq!(
        published.json(),
        json!({"dashboard_id": saved, "version": 4}),
        "{published:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn versions_written_before_the_first_publish_are_based_on_no_saved_version() {
    let Some(Setup { world, api, .. }) = setup(RunCaps::default()).await else {
        return;
    };
    let base = api.url.as_str();
    let editor = world.person(TenantId::generate(), TenantRole::Editor);
    let as_editor = world.token(&editor);
    let conversation = world.conversation(&editor).await;
    let draft = world
        .create_draft(&editor, conversation, &Content::new("First"))
        .await
        .dashboard_id;
    world
        .write_draft(
            &editor,
            conversation,
            draft,
            1,
            None,
            &Content::new("Second"),
        )
        .await
        .expect("version 2");
    let publish = |version: u32| {
        let token = as_editor.clone();
        async move {
            call(
                base,
                Method::POST,
                &action(draft, "publish"),
                Some(&token),
                Some(&json!({"version": version})),
            )
            .await
        }
    };
    let saved_base = || {
        let token = as_editor.clone();
        async move {
            call(base, Method::GET, &dashboard(draft), Some(&token), None)
                .await
                .json()["dashboard"]["saved"]
                .clone()
        }
    };

    // Version 2 becomes the saved dashboard; the draft's next version starts from it.
    let first = publish(2).await;
    assert_eq!(first.status, 201, "{first:?}");
    let saved = DashboardId::from_uuid(
        Uuid::try_parse(first.json()["dashboard_id"].as_str().expect("an ID")).expect("a UUID"),
    );
    assert_eq!(
        saved_base().await,
        json!({"dashboard_id": saved, "version": 1})
    );
    // Neither version 1, written before it, nor version 2 again is published over it.
    for version in [1, 2] {
        let refused = publish(version).await;
        assert_error(&refused, 409, "conflict", "dashboard.saved_changed");
        assert_eq!(refused.json()["details"]["latest"], 1);
    }
    // Restoring version 1 brings back content based on no saved version: the draft says so,
    // and neither the copy nor the tool's next version written on it publishes.
    let restored = restore(base, &as_editor, draft, 1, 2).await;
    assert_eq!(
        restored.json(),
        json!({"dashboard_id": draft, "version": 3}),
        "{restored:?}"
    );
    assert_eq!(saved_base().await, json!({"dashboard_id": saved}));
    world
        .write_draft(
            &editor,
            conversation,
            draft,
            3,
            None,
            &Content::new("Fourth"),
        )
        .await
        .expect("version 4");
    for version in [3, 4] {
        let refused = publish(version).await;
        assert_error(&refused, 409, "conflict", "dashboard.saved_changed");
    }
    assert_eq!(version_count(&world, saved).await, 1);
    // Once the tool merged the saved dashboard's version 1 in, the draft publishes again.
    world
        .write_draft(
            &editor,
            conversation,
            draft,
            4,
            Some(1),
            &Content::new("Fifth"),
        )
        .await
        .expect("version 5");
    let published = publish(5).await;
    assert_eq!(
        published.json(),
        json!({"dashboard_id": saved, "version": 2}),
        "{published:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn datasets_run_as_the_viewer_who_opened_the_dashboard() {
    let Some(Setup { world, peers, api }) = setup(RunCaps::default()).await else {
        return;
    };
    let base = api.url.as_str();
    let tenant = TenantId::generate();
    let editor = world.person(tenant, TenantRole::Editor);
    let viewer = world.person(tenant, TenantRole::Viewer);
    let content = Content::new("Plant");
    let (_, _, saved) = world.saved(&editor, &content).await;
    let as_viewer = world.token(&viewer);

    let mut results = Vec::new();
    for name in DATASETS {
        let answer = call(
            base,
            Method::POST,
            &action(saved, "dataset-runs"),
            Some(&as_viewer),
            Some(&run_body(1, name)),
        )
        .await;
        assert_eq!(answer.status, 200, "{name}: {answer:?}");
        assert_no_store(&answer);
        results.push(answer.json());
    }
    // The tables.
    assert_eq!(results[0]["dataset"], "daily");
    assert_eq!(
        results[0]["columns"],
        json!([{"name": "day", "type": "time"}, {"name": "kwh", "type": "number"}])
    );
    assert_eq!(results[0]["row_count"], 3);
    assert_eq!(results[0]["meta"]["source"]["kind"], "sql");
    assert_eq!(results[1]["dataset"], "stats");
    assert_eq!(results[1]["row_count"], 1);
    assert_eq!(results[1]["rows"][0][0], json!(dashboard_world::DEVICE));
    assert_eq!(results[2]["dataset"], "usage");
    assert_eq!(results[2]["row_count"], 3);

    // telemetry-query got the viewer's token: the viewer's user, tenant and role, addressed to
    // it, held by the assistant. Never the author's.
    let calls = peers.calls();
    let paths: Vec<&str> = calls.iter().map(|call| call.path.as_str()).collect();
    assert_eq!(paths, [SQL_PATH, STATISTICS_PATH, USAGE_PATH]);
    for call in &calls {
        assert_eq!(call.claims.sub, viewer.claims.sub, "{call:?}");
        assert_eq!(
            call.claims.tenant,
            Some(TenantGrant {
                id: tenant,
                role: TenantRole::Viewer
            })
        );
        assert_eq!(call.claims.aud, ServiceName::TelemetryQuery);
        assert_eq!(
            call.claims.act,
            Some(Actor {
                sub: ServiceName::Assistant
            })
        );
    }
    // The stored queries, as stored: the SQL with the dataset's row limit, the semantic requests
    // byte for byte.
    let stored = serde_json::to_value(&content.spec).expect("the spec as JSON");
    assert_eq!(
        calls[0].body,
        json!({"sql": stored["datasets"][0]["source"]["sql"], "max_rows": 2000})
    );
    assert_eq!(calls[1].body, stored["datasets"][1]["source"]["request"]);
    assert_eq!(calls[2].body, stored["datasets"][2]["source"]["request"]);
    assert!(
        peers
            .exchanged()
            .iter()
            .all(|claims| claims.sub == viewer.claims.sub)
    );

    // The editor's run is the editor's.
    let as_editor = world.token(&editor);
    let answer = call(
        base,
        Method::POST,
        &action(saved, "dataset-runs"),
        Some(&as_editor),
        Some(&run_body(1, "daily")),
    )
    .await;
    assert_eq!(answer.status, 200, "{answer:?}");
    let last = peers.calls().pop().expect("the editor's run");
    assert_eq!(last.claims.sub, editor.claims.sub);
    assert_eq!(
        last.claims.tenant.map(|grant| grant.role),
        Some(TenantRole::Editor)
    );

    // A dataset or version the dashboard does not have.
    let unknown = call(
        base,
        Method::POST,
        &action(saved, "dataset-runs"),
        Some(&as_viewer),
        Some(&run_body(1, "nope")),
    )
    .await;
    assert_error(
        &unknown,
        422,
        "validation_failed",
        "dashboard.dataset_unknown",
    );
    let missing = call(
        base,
        Method::POST,
        &action(saved, "dataset-runs"),
        Some(&as_viewer),
        Some(&run_body(2, "daily")),
    )
    .await;
    assert_error(&missing, 404, "not_found", "dashboard.version_not_found");
    assert_eq!(peers.calls().len(), 4, "neither reached telemetry-query");
}

#[tokio::test(flavor = "multi_thread")]
async fn more_rows_than_the_dataset_allows_are_refused_with_the_limit() {
    let Some(Setup { world, peers, api }) = setup(RunCaps::default()).await else {
        return;
    };
    let base = api.url.as_str();
    let viewer = world.person(TenantId::generate(), TenantRole::Viewer);
    let editor = world.person(viewer.tenant(), TenantRole::Editor);
    let (_, _, saved) = world.saved(&editor, &Content::new("Rows")).await;
    let token = world.token(&viewer);
    let path = action(saved, "dataset-runs");

    // telemetry-query refuses the query at the limit the run sent (2000, the dataset's).
    let refusal = json!({
        "code": "validation_failed",
        "message_key": "error.validation_failed",
        "details": {"class": "too_many_rows", "max": 2000},
    });
    peers.script(Arc::new(move |_| {
        let refusal = refusal.clone();
        Box::pin(async move { Some((422, refusal)) })
    }));
    let refused = call(
        base,
        Method::POST,
        &path,
        Some(&token),
        Some(&run_body(1, "daily")),
    )
    .await;
    assert_error(
        &refused,
        422,
        "validation_failed",
        "dashboard.dataset_too_many_rows",
    );
    assert_eq!(refused.json()["details"], json!({"max": 2000}));
    assert_eq!(peers.calls()[0].body["max_rows"], 2000);

    // An answer with more rows than asked for is refused all the same, never cut short.
    peers.script(Arc::new(|_| Box::pin(async { None })));
    peers.sql_rows(2001);
    let refused = call(
        base,
        Method::POST,
        &path,
        Some(&token),
        Some(&run_body(1, "daily")),
    )
    .await;
    assert_error(
        &refused,
        422,
        "validation_failed",
        "dashboard.dataset_too_many_rows",
    );
    assert_eq!(refused.json()["details"], json!({"max": 2000}));
    peers.sql_rows(2000);
    let full = call(
        base,
        Method::POST,
        &path,
        Some(&token),
        Some(&run_body(1, "daily")),
    )
    .await;
    assert_eq!(full.status, 200, "{}", full.status);
    assert_eq!(full.json()["row_count"], 2000);
}

/// Runs datasets as the `dashboard` tool does: with a question's delegation, which gives a token
/// for the user like the identity service's exchange of a run delegation.
struct QuestionDelegation {
    issuer: Arc<TestIssuer>,
    user: InternalClaims,
}

impl Delegation for QuestionDelegation {
    fn token_for(
        &self,
        audience: ServiceName,
    ) -> ModelFuture<'_, Result<InternalToken, DelegationError>> {
        let iat = i64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("after the epoch")
                .as_secs(),
        )
        .expect("seconds fit");
        let mut claims = self.user.clone();
        claims.aud = audience;
        claims.iat = iat;
        claims.exp = iat + 60;
        claims.act = Some(Actor {
            sub: ServiceName::Assistant,
        });
        let token = self.issuer.token(&claims);
        Box::pin(async move { InternalToken::new(token).map_err(|_| DelegationError::Unavailable) })
    }
}

/// Holds the telemetry calls of the tenants in `held` until `gate` gives permits.
fn holding(gate: &Arc<Semaphore>, held: &Arc<Mutex<HashSet<TenantId>>>) -> Script {
    let (gate, held) = (Arc::clone(gate), Arc::clone(held));
    Arc::new(move |call: TelemetryCall| {
        let gate = Arc::clone(&gate);
        let hold = call.tenant().is_some_and(|tenant| {
            held.lock()
                .unwrap_or_else(PoisonError::into_inner)
                .contains(&tenant)
        });
        Box::pin(async move {
            if hold {
                gate.acquire().await.expect("the gate stays open").forget();
            }
            None
        })
    })
}

/// A tenant with a saved dashboard and a viewer.
async fn tenant_with_dashboard(world: &World) -> (Person, DashboardId) {
    let tenant = TenantId::generate();
    let editor = world.person(tenant, TenantRole::Editor);
    let (_, _, saved) = world.saved(&editor, &Content::new("Capped")).await;
    (world.person(tenant, TenantRole::Viewer), saved)
}

#[tokio::test(flavor = "multi_thread")]
async fn the_caps_refuse_the_61st_run_in_a_minute_and_a_third_at_a_time_per_tenant() {
    let (clock, elapsed_ms) = manual_clock();
    // No wait for a slot, so the third concurrent run is refused at once.
    let caps = RunCaps::new(RUNS_PER_MINUTE, CONCURRENT_RUNS, Duration::ZERO, clock);
    let Some(Setup { world, peers, api }) = setup(caps).await else {
        return;
    };
    let base = api.url.clone();
    let (busy, busy_saved) = tenant_with_dashboard(&world).await;
    let (other, other_saved) = tenant_with_dashboard(&world).await;
    let (parallel, parallel_saved) = tenant_with_dashboard(&world).await;
    let (tool_user, tool_saved) = tenant_with_dashboard(&world).await;
    let gate = Arc::new(Semaphore::new(0));
    let held = Arc::new(Mutex::new(HashSet::new()));
    peers.script(holding(&gate, &held));
    let hold = |tenant: TenantId, on: bool| {
        let mut held = held.lock().unwrap_or_else(PoisonError::into_inner);
        if on {
            held.insert(tenant);
        } else {
            held.remove(&tenant);
        }
    };
    let run = |person: &Person, saved: DashboardId| {
        let (base, token, path) = (
            base.clone(),
            world.token(person),
            action(saved, "dataset-runs"),
        );
        tokio::spawn(async move {
            call(
                &base,
                Method::POST,
                &path,
                Some(&token),
                Some(&run_body(1, "daily")),
            )
            .await
        })
    };
    let calls_of = |tenant: TenantId| {
        peers
            .calls()
            .iter()
            .filter(|call| call.tenant() == Some(tenant))
            .count()
    };

    // Another tenant has a run in flight the whole time.
    hold(other.tenant(), true);
    let in_flight = run(&other, other_saved);
    peers.wait_calls(1).await;

    // 60 runs in a minute, then the 61st is refused until the first leaves the minute.
    for index in 0..RUNS_PER_MINUTE {
        let answer = run(&busy, busy_saved).await.expect("the run");
        assert_eq!(answer.status, 200, "run {index}: {answer:?}");
    }
    let refused = run(&busy, busy_saved).await.expect("the 61st run");
    assert_error(&refused, 429, "rate_limited", "dashboard.runs_limited");
    assert_eq!(refused.header("retry-after"), Some("60"));
    assert_eq!(refused.json()["details"], json!({"retry_after": 60}));
    assert_eq!(
        calls_of(busy.tenant()),
        RUNS_PER_MINUTE,
        "the refused run called nobody"
    );

    // The other tenant's run was never in its way, and it runs again.
    hold(other.tenant(), false);
    gate.add_permits(1);
    let answer = tokio::time::timeout(dashboard_world::WAIT, in_flight)
        .await
        .expect("the other tenant's run ends once released")
        .expect("the run");
    assert_eq!(answer.status, 200, "{answer:?}");
    assert_eq!(run(&other, other_saved).await.expect("the run").status, 200);

    // A minute after the first run, the busy tenant runs again.
    elapsed_ms.store(60_000, Ordering::SeqCst);
    assert_eq!(run(&busy, busy_saved).await.expect("the run").status, 200);

    // Two runs at a time: a third is refused for a second, then all run.
    hold(parallel.tenant(), true);
    let first = run(&parallel, parallel_saved);
    let second = run(&parallel, parallel_saved);
    let before = peers.calls().len();
    let deadline = Instant::now() + dashboard_world::WAIT;
    while calls_of(parallel.tenant()) < 2 {
        assert!(
            Instant::now() < deadline,
            "two runs of the tenant did not start"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let third = run(&parallel, parallel_saved).await.expect("the third run");
    assert_error(&third, 429, "rate_limited", "dashboard.runs_limited");
    assert_eq!(third.header("retry-after"), Some("1"));
    assert_eq!(peers.calls().len(), before + 2, "the third called nobody");
    hold(parallel.tenant(), false);
    gate.add_permits(2);
    for running in [first, second] {
        let answer = tokio::time::timeout(dashboard_world::WAIT, running)
            .await
            .expect("the held runs end once released")
            .expect("the run");
        assert_eq!(answer.status, 200, "{answer:?}");
    }
    assert_eq!(
        run(&parallel, parallel_saved)
            .await
            .expect("the run")
            .status,
        200
    );

    // The `dashboard` tool's runs count against the same caps: after 60 of its runs, the
    // user's own run in the browser is refused.
    let delegation = QuestionDelegation {
        issuer: Arc::clone(&world.issuer),
        user: tool_user.claims.clone(),
    };
    let caller = Caller {
        delegation: &delegation,
        tenant_id: tool_user.tenant(),
        operation: Operation::Tool("dashboard"),
        run_id: Some(Uuid::now_v7()),
        conversation_id: Some(Uuid::now_v7()),
    };
    let dataset: Dataset = spec().dataset("daily").cloned().expect("the SQL dataset");
    for index in 0..RUNS_PER_MINUTE {
        api.runner
            .run(&caller, &dataset)
            .await
            .unwrap_or_else(|error| panic!("the tool's run {index}: {error:?}"));
    }
    let refused = run(&tool_user, tool_saved).await.expect("the run");
    assert_error(&refused, 429, "rate_limited", "dashboard.runs_limited");
}

/// A run waiting for telemetry-query's answer holds only its tenant's slot, not the replica's
/// memory: while four tenants have both of their runs waiting for slow answers (as many runs as
/// the replica once allowed in all), a fifth tenant's run is answered at once.
#[tokio::test(flavor = "multi_thread")]
async fn slow_answers_of_other_tenants_do_not_stop_a_run() {
    let (clock, _) = manual_clock();
    // No wait for a slot: a run that could not start at once would be refused.
    let caps = RunCaps::new(RUNS_PER_MINUTE, CONCURRENT_RUNS, Duration::ZERO, clock);
    let Some(Setup { world, peers, api }) = setup(caps).await else {
        return;
    };
    let base = api.url.clone();
    let gate = Arc::new(Semaphore::new(0));
    let held = Arc::new(Mutex::new(HashSet::new()));
    peers.script(holding(&gate, &held));
    let run = |person: &Person, saved: DashboardId| {
        let (base, token, path) = (
            base.clone(),
            world.token(person),
            action(saved, "dataset-runs"),
        );
        tokio::spawn(async move {
            call(
                &base,
                Method::POST,
                &path,
                Some(&token),
                Some(&run_body(1, "daily")),
            )
            .await
        })
    };
    let mut waiting = Vec::new();
    for _ in 0..4 {
        let (viewer, saved) = tenant_with_dashboard(&world).await;
        held.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(viewer.tenant());
        for _ in 0..CONCURRENT_RUNS {
            waiting.push(run(&viewer, saved));
        }
    }
    peers.wait_calls(4 * CONCURRENT_RUNS).await;

    let (free, free_saved) = tenant_with_dashboard(&world).await;
    let answer = run(&free, free_saved).await.expect("the run");
    assert_eq!(answer.status, 200, "{answer:?}");

    gate.add_permits(4 * CONCURRENT_RUNS);
    for running in waiting {
        let answer = tokio::time::timeout(dashboard_world::WAIT, running)
            .await
            .expect("the held runs end once released")
            .expect("the run");
        assert_eq!(answer.status, 200, "{answer:?}");
    }
}

/// A run's answer holds the result's memory until its body has been handed over or dropped, not
/// only while the run goes on: an answer whose body is not read yet keeps the body's length
/// reserved for the tenant; reading the whole body, or dropping the answer unread, gives it
/// back. Served over TCP, the memory is back once the client has the whole body.
#[tokio::test(flavor = "multi_thread")]
async fn a_run_answer_holds_its_memory_until_its_body_is_written_or_dropped() {
    let Some(Setup { world, peers, api }) = setup(RunCaps::default()).await else {
        return;
    };
    let runner = Arc::new(DatasetRunner::new(
        Some(telemetry_client(&peers)),
        RunCaps::default(),
    ));
    let routes = world.routes(&peers, Arc::clone(&runner), true);
    let (viewer, saved) = tenant_with_dashboard(&world).await;
    let memory = runner.memory();
    let request = || {
        axum::http::Request::builder()
            .method(Method::POST)
            .uri(action(saved, "dataset-runs"))
            .header(INTERNAL_IDENTITY_HEADER, world.token(&viewer))
            .header(CONTENT_TYPE, "application/json")
            .body(axum::body::Body::from(
                serde_json::to_vec(&run_body(1, "daily")).expect("JSON"),
            ))
            .expect("a request")
    };

    let answer = routes.clone().oneshot(request()).await.expect("an answer");
    assert_eq!(answer.status(), 200);
    let held = memory.held_by(viewer.tenant());
    assert!(held > 0, "the unread answer holds the result's memory");
    assert_eq!(memory.used(), held);
    let body = axum::body::to_bytes(answer.into_body(), usize::MAX)
        .await
        .expect("the body");
    assert_eq!(held, body.len(), "it holds the body's length");
    let result: Value = serde_json::from_slice(&body).expect("JSON");
    assert_eq!(result["dataset"], "daily");
    drop(body);
    assert_eq!(memory.used(), 0, "the body read and let go");

    let answer = routes.clone().oneshot(request()).await.expect("an answer");
    assert_eq!(answer.status(), 200);
    assert!(memory.held_by(viewer.tenant()) > 0);
    drop(answer);
    assert_eq!(memory.used(), 0, "the answer dropped unread");

    // Served over TCP: once the client has the whole body, the server has handed it over.
    let (other, other_saved) = tenant_with_dashboard(&world).await;
    let answer = call(
        &api.url,
        Method::POST,
        &action(other_saved, "dataset-runs"),
        Some(&world.token(&other)),
        Some(&run_body(1, "daily")),
    )
    .await;
    assert_eq!(answer.status, 200, "{answer:?}");
    let deadline = Instant::now() + dashboard_world::WAIT;
    while api.runner.memory().used() > 0 {
        assert!(
            Instant::now() < deadline,
            "the server did not give the memory back after writing the body"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The rows and the text cell of the wide answer: a result of about 1.5 MB, far more than the
/// small socket buffers below and the connection's own write buffer (about 400 KiB) hold.
const WIDE_ROWS: usize = 1500;
const WIDE_CELL: usize = 1000;

/// The send buffer of the server's connections and the receive buffer of the reader that stops.
const SMALL_BUFFER: usize = 4096;

/// Waits until `memory` holds nothing, failing with `what` after a generous bound.
async fn wait_until_released(memory: &ResultMemory, what: &str) {
    let deadline = Instant::now() + dashboard_world::WAIT;
    while memory.used() > 0 {
        assert!(Instant::now() < deadline, "{what} was not released");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Reads an HTTP answer's status line and headers, a little at a time: the head, and the few
/// bytes of the body read with it.
async fn read_head(stream: &mut TcpStream) -> (String, Vec<u8>) {
    let mut read = Vec::new();
    let mut buffer = [0_u8; 1024];
    loop {
        if let Some(end) = read.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
            let body = read.split_off(end + 4);
            return (String::from_utf8(read).expect("a text head"), body);
        }
        let count = stream
            .read(&mut buffer)
            .await
            .expect("read the answer's head");
        assert!(count > 0, "the connection ended before the answer's head");
        read.extend_from_slice(&buffer[..count]);
    }
}

/// The `content-length` of an answer's `head`.
fn content_length(head: &str) -> usize {
    head.lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().ok())
                .flatten()
        })
        .unwrap_or_else(|| panic!("no content-length in {head}"))
}

/// A server's listener whose connections have a small send buffer and report when the server
/// drops them.
struct WatchedListener {
    listener: TcpListener,
    dropped: mpsc::UnboundedSender<Instant>,
}

/// A server connection that reports the instant the server drops it. It writes as the
/// `TcpStream` it wraps does, vectored included, so the server buffers its answers as it does
/// on a plain `TcpListener`.
struct WatchedConnection {
    stream: TcpStream,
    dropped: mpsc::UnboundedSender<Instant>,
}

impl axum::serve::Listener for WatchedListener {
    type Io = WatchedConnection;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (WatchedConnection, SocketAddr) {
        let (stream, address) = self.listener.accept().await.expect("accept");
        socket2::SockRef::from(&stream)
            .set_send_buffer_size(SMALL_BUFFER)
            .expect("a small send buffer");
        let dropped = self.dropped.clone();
        (WatchedConnection { stream, dropped }, address)
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }
}

impl Drop for WatchedConnection {
    fn drop(&mut self) {
        let _ = self.dropped.send(Instant::now());
    }
}

impl AsyncRead for WatchedConnection {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().stream).poll_read(cx, buf)
    }
}

impl AsyncWrite for WatchedConnection {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().stream).poll_write(cx, buf)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().stream).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.stream.is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().stream).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().stream).poll_shutdown(cx)
    }
}

/// Reads until the connection ends, cleanly or not.
async fn read_to_end(stream: &mut TcpStream, into: &mut Vec<u8>) {
    let mut buffer = vec![0_u8; 64 << 10];
    loop {
        match stream.read(&mut buffer).await {
            Ok(0) | Err(_) => return,
            Ok(count) => into.extend_from_slice(&buffer[..count]),
        }
    }
}

/// A run's answer ends when the connection has not taken the whole body within its bound
/// (`HELD_BODY_TIMEOUT`, shortened here). A reader that reads gets the whole body, byte for byte,
/// and the memory is back once the connection has taken it. A reader that stops reading, on a
/// connection whose socket buffers then fill, loses its connection at the bound while it still
/// reads nothing, the result's memory is released by then, and reading again it gets fewer bytes
/// than the answer's length, then the end.
#[tokio::test(flavor = "multi_thread")]
async fn a_reader_that_stops_loses_the_answer_and_the_connection_at_the_bound() {
    let Some(Setup { world, peers, api }) = setup(RunCaps::default()).await else {
        return;
    };
    peers.script(Arc::new(|call| {
        Box::pin(async move { Some((200, wide_sql_answer(&call.body, WIDE_ROWS, WIDE_CELL))) })
    }));
    let (viewer, saved) = tenant_with_dashboard(&world).await;
    let path = action(saved, "dataset-runs");

    // A reader that reads, from the API with the service's bound.
    let answer = call(
        &api.url,
        Method::POST,
        &path,
        Some(&world.token(&viewer)),
        Some(&run_body(1, "daily")),
    )
    .await;
    assert_eq!(answer.status, 200, "{}", answer.text);
    assert!(answer.text.len() > 1_000_000, "{}", answer.text.len());
    assert_eq!(
        answer.header("content-length"),
        Some(answer.text.len().to_string().as_str())
    );
    let result: DatasetResult = serde_json::from_str(&answer.text).expect("a result");
    assert_eq!(result.rows.len(), WIDE_ROWS);
    assert_eq!(
        serde_json::to_string(&result).expect("JSON"),
        answer.text,
        "the whole body, byte for byte as the server wrote the result"
    );
    wait_until_released(
        api.runner.memory(),
        "the memory of an answer its reader read",
    )
    .await;

    // A reader that stops, from a server whose answers let their results go after a short bound,
    // on connections with small socket buffers.
    let runner = Arc::new(DatasetRunner::new(
        Some(telemetry_client(&peers)),
        RunCaps::default(),
    ));
    let bound = Duration::from_secs(1);
    let routes = world.routes_bounded(&peers, Arc::clone(&runner), true, bound);
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("the address");
    let (dropped, mut connections_dropped) = mpsc::unbounded_channel();
    let listener = WatchedListener { listener, dropped };
    let server = tokio::spawn(async move { axum::serve(listener, routes).await });
    let socket = TcpSocket::new_v4().expect("a socket");
    socket
        .set_recv_buffer_size(u32::try_from(SMALL_BUFFER).expect("small"))
        .expect("a small receive buffer");
    let mut stream = socket.connect(address).await.expect("connect");
    let asked = Instant::now();
    let request = serde_json::to_vec(&run_body(1, "daily")).expect("JSON");
    let head = format!(
        "POST {path} HTTP/1.1\r\nhost: {address}\r\n{INTERNAL_IDENTITY_HEADER}: {}\r\n\
         content-type: application/json\r\ncontent-length: {}\r\n\r\n",
        world.token(&viewer),
        request.len()
    );
    stream
        .write_all(head.as_bytes())
        .await
        .expect("the request");
    stream
        .write_all(&request)
        .await
        .expect("the request's body");
    let (status_and_headers, mut body) =
        tokio::time::timeout(dashboard_world::WAIT, read_head(&mut stream))
            .await
            .expect("the answer's head");
    assert!(
        status_and_headers.starts_with("HTTP/1.1 200 "),
        "{status_and_headers}"
    );
    let length = content_length(&status_and_headers);
    assert!(length > 1_000_000, "{length}");

    // The reader reads nothing more. At the bound the server drops the connection, its socket
    // buffers still full, and the result's memory is back by then.
    let closed = tokio::time::timeout(dashboard_world::WAIT, connections_dropped.recv())
        .await
        .expect("the server dropped the connection of the reader that stopped, at the bound")
        .expect("the server's listener");
    assert!(
        closed.duration_since(asked) >= bound,
        "the connection was dropped before the bound"
    );
    assert_eq!(runner.memory().used(), 0, "the memory is back at the bound");

    // Reading again, the reader gets what the socket buffers held, then the end: never the whole
    // answer.
    tokio::time::timeout(dashboard_world::WAIT, read_to_end(&mut stream, &mut body))
        .await
        .expect("the connection ends");
    assert!(
        body.len() < length,
        "the reader that stopped lost the answer: {} of {length} bytes",
        body.len()
    );
    server.abort();
}

/// One tenant holds at most its share of the replica's memory: with its share taken (by its
/// answers not yet read, or a write), the tenant's run is refused once telemetry-query answered,
/// 429 `dashboard.runs_limited` with `Retry-After: 1`, without reading the answer, and it stays
/// counted in the minute because it used telemetry-query; another tenant runs meanwhile, and
/// the tenant runs again once its memory is free.
#[tokio::test(flavor = "multi_thread")]
async fn a_tenant_holds_at_most_its_share_and_a_run_refused_for_memory_is_counted() {
    let (clock, _) = manual_clock();
    // Two runs a minute, so the count shows; no waits.
    let caps = RunCaps::new(2, CONCURRENT_RUNS, Duration::ZERO, clock);
    let Some(Setup { world, peers, api }) = setup(caps).await else {
        return;
    };
    let memory = api.runner.memory();
    let (busy, busy_saved) = tenant_with_dashboard(&world).await;
    let (other, other_saved) = tenant_with_dashboard(&world).await;
    let run = |person: &Person, saved: DashboardId| {
        let (base, token, path) = (
            api.url.clone(),
            world.token(person),
            action(saved, "dataset-runs"),
        );
        async move {
            call(
                &base,
                Method::POST,
                &path,
                Some(&token),
                Some(&run_body(1, "daily")),
            )
            .await
        }
    };
    let calls_of = |tenant: TenantId| {
        peers
            .calls()
            .iter()
            .filter(|call| call.tenant() == Some(tenant))
            .count()
    };

    let taken = memory
        .reserve(busy.tenant(), memory.per_tenant() - 1, Duration::ZERO)
        .await
        .expect("an idle replica");
    let refused = run(&busy, busy_saved).await;
    assert_error(&refused, 429, "rate_limited", "dashboard.runs_limited");
    assert_eq!(refused.header("retry-after"), Some("1"));
    assert_eq!(refused.json()["details"], json!({"retry_after": 1}));
    assert_eq!(calls_of(busy.tenant()), 1, "it called telemetry-query");
    assert_eq!(
        memory.held_by(busy.tenant()),
        memory.per_tenant() - 1,
        "the refused run holds nothing"
    );

    let answer = run(&other, other_saved).await;
    assert_eq!(answer.status, 200, "another tenant runs: {answer:?}");

    drop(taken);
    let answer = run(&busy, busy_saved).await;
    assert_eq!(
        answer.status, 200,
        "the tenant's memory is free: {answer:?}"
    );
    let limited = run(&busy, busy_saved).await;
    assert_error(&limited, 429, "rate_limited", "dashboard.runs_limited");
    assert_eq!(
        limited.header("retry-after"),
        Some("60"),
        "the run refused for memory counted in the minute"
    );
    assert_eq!(memory.used(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn telemetry_querys_refusals_reach_the_user_only_as_closed_keys() {
    let Some(Setup { world, peers, api }) = setup(RunCaps::default()).await else {
        return;
    };
    let base = api.url.as_str();
    let viewer = world.person(TenantId::generate(), TenantRole::Viewer);
    let editor = world.person(viewer.tenant(), TenantRole::Editor);
    let (_, _, saved) = world.saved(&editor, &Content::new("Refusals")).await;
    let token = world.token(&viewer);
    let secret = format!("downstream secret {}", Uuid::now_v7());
    let next: Arc<Mutex<Option<(u16, Value)>>> = Arc::new(Mutex::new(None));
    peers.script({
        let next = Arc::clone(&next);
        Arc::new(move |_| {
            let answer = next.lock().unwrap_or_else(PoisonError::into_inner).clone();
            Box::pin(async move { answer })
        })
    });
    let error = |code: &str, key: &str, details: Value| json!({"code": code, "message_key": key, "details": details});
    let cases: Vec<(&str, u16, Value, u16, &str, &str, Value)> = vec![
        (
            "daily",
            429,
            error(
                "rate_limited",
                "error.rate_limited",
                json!({"note": secret}),
            ),
            429,
            "rate_limited",
            "dashboard.query_quota_exhausted",
            Value::Null,
        ),
        (
            "daily",
            404,
            error(
                "not_found",
                "telemetry.account_not_ready",
                json!({"note": secret}),
            ),
            503,
            "unavailable",
            "telemetry.account_not_ready",
            Value::Null,
        ),
        (
            "stats",
            403,
            error(
                "quota_exceeded",
                "telemetry.outside_queryable_period",
                json!({"earliest": "2026-10-19T00:00:00+08:00", "note": secret}),
            ),
            403,
            "quota_exceeded",
            "telemetry.outside_queryable_period",
            json!({"earliest": "2026-10-19T00:00:00+08:00"}),
        ),
        (
            "stats",
            422,
            error(
                "validation_failed",
                "telemetry.too_many_points",
                json!({"parameter": "interval", "max": 10_000, "note": secret}),
            ),
            422,
            "validation_failed",
            "telemetry.too_many_points",
            json!({"parameter": "interval", "max": 10_000}),
        ),
        (
            "usage",
            422,
            error(
                "validation_failed",
                "telemetry.something_new",
                json!({"note": secret}),
            ),
            422,
            "validation_failed",
            "dashboard.dataset_refused",
            Value::Null,
        ),
        (
            "daily",
            422,
            error(
                "validation_failed",
                "error.validation_failed",
                json!({"class": "function_not_allowed", "function": "dictGet", "note": secret}),
            ),
            422,
            "validation_failed",
            "dashboard.dataset_refused",
            Value::Null,
        ),
        (
            "daily",
            504,
            error("timeout", "error.timeout", json!({"note": secret})),
            504,
            "timeout",
            "dashboard.dataset_timeout",
            Value::Null,
        ),
        (
            "daily",
            500,
            error("internal", "error.internal", json!({"note": secret})),
            503,
            "unavailable",
            "dashboard.dataset_failed",
            Value::Null,
        ),
        (
            "usage",
            200,
            json!({"answer": secret}),
            503,
            "unavailable",
            "dashboard.dataset_failed",
            Value::Null,
        ),
    ];
    for (dataset, status, body, expected, code, key, details) in cases {
        *next.lock().unwrap_or_else(PoisonError::into_inner) = Some((status, body));
        let answer = call(
            base,
            Method::POST,
            &action(saved, "dataset-runs"),
            Some(&token),
            Some(&run_body(1, dataset)),
        )
        .await;
        assert_error(&answer, expected, code, key);
        assert_eq!(answer.json()["details"], details, "{answer:?}");
        assert!(!answer.text.contains(&secret), "{answer:?}");
        assert_eq!(
            answer.header("retry-after"),
            None,
            "only the caps say when to retry"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn without_the_token_exchange_a_run_fails_and_nothing_is_called() {
    let Some(world) = world().await else { return };
    let peers = Peers::start(Arc::clone(&world.issuer)).await;
    let api = world.api_with(&peers, RunCaps::default(), false).await;
    let viewer = world.person(TenantId::generate(), TenantRole::Viewer);
    let editor = world.person(viewer.tenant(), TenantRole::Editor);
    let (_, _, saved) = world.saved(&editor, &Content::new("No exchange")).await;
    let answer = call(
        &api.url,
        Method::POST,
        &action(saved, "dataset-runs"),
        Some(&world.token(&viewer)),
        Some(&run_body(1, "daily")),
    )
    .await;
    assert_error(&answer, 503, "unavailable", "dashboard.dataset_failed");
    assert!(peers.calls().is_empty());
    assert!(peers.exchanged().is_empty());
    // The rest of the API works without it.
    let opened = call(
        &api.url,
        Method::GET,
        &dashboard(saved),
        Some(&world.token(&viewer)),
        None,
    )
    .await;
    assert_eq!(opened.status, 200, "{opened:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn opening_a_dashboard_and_running_all_its_datasets_calls_no_model() {
    let Some(world) = world().await else { return };
    let peers = Peers::start(Arc::clone(&world.issuer)).await;
    // Every peer the binary has, the AI gateway included, is the stand-in.
    let assistant = Binary::start(&world.product, &world.issuer, &peers.url).await;
    let tenant = TenantId::generate();
    let editor = world.person(tenant, TenantRole::Editor);
    let viewer = world.person(tenant, TenantRole::Viewer);
    let title = format!("Plant energy {}", Uuid::now_v7().simple());
    let content = Content::new(&title);
    let (_, _, saved) = world.saved(&editor, &content).await;
    let token = world.token(&viewer);
    let base = assistant.url.as_str();

    let list = call(base, Method::GET, DASHBOARDS, Some(&token), None).await;
    assert_eq!(list.json()["dashboards"][0]["dashboard_id"], json!(saved));
    let opened = call(base, Method::GET, &dashboard(saved), Some(&token), None).await;
    assert_eq!(opened.json()["version"]["title"], json!(title));
    let versions = call(
        base,
        Method::GET,
        &action(saved, "versions"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(versions.status, 200, "{versions:?}");
    // Every dataset, twice: nothing is kept between runs, so each run reads again.
    for _ in 0..2 {
        for name in DATASETS {
            let answer = call(
                base,
                Method::POST,
                &action(saved, "dataset-runs"),
                Some(&token),
                Some(&run_body(1, name)),
            )
            .await;
            assert_eq!(answer.status, 200, "{name}: {answer:?}");
        }
    }
    let paths: Vec<String> = peers.calls().into_iter().map(|call| call.path).collect();
    assert_eq!(
        paths,
        [
            SQL_PATH,
            STATISTICS_PATH,
            USAGE_PATH,
            SQL_PATH,
            STATISTICS_PATH,
            USAGE_PATH
        ]
    );
    assert!(
        peers
            .calls()
            .iter()
            .all(|call| call.claims.sub == viewer.claims.sub)
    );
    // No AI gateway call, nor any other.
    assert!(peers.unexpected().is_empty(), "{:?}", peers.unexpected());
    // The logs carry no content: no title, SQL or page.
    let sql = serde_json::to_value(&content.spec).expect("JSON")["datasets"][0]["source"]["sql"]
        .as_str()
        .expect("the SQL")
        .to_owned();
    for text in [assistant.stdout.text(), assistant.stderr.text()] {
        assert!(!text.contains(&title), "a log line has the title");
        assert!(!text.contains(&sql), "a log line has the SQL");
        assert!(!text.contains(&content.page), "a log line has the page");
    }
}
