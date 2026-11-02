//! The dashboard drafts of a shared conversation (Laysi/trustiot-v2#650, with the dashboards of
//! Laysi/trustiot-v2#150; the decision in Laysi/trustiot-v2#150's design record of 2026-10-26:
//! 「分享的讀者：唯讀看到對話的草稿，資料集以讀者的身分執行」), through the public API served
//! in the test process with the stand-in token exchange and telemetry-query of `dashboard_world`.
//!
//! - While the owner shares the conversation, every other member of the tenant, whatever the
//!   role, lists and opens its drafts and their versions and runs their datasets as themselves:
//!   telemetry-query gets the reader's token, never the owner's. The view says the reader may do
//!   nothing, and every write is refused (publishing and restoring with
//!   `dashboard.owner_required`), writing nothing.
//! - Unsharing or deleting the conversation takes the drafts from the readers at once: every
//!   route answers the same 404 as for a dashboard or conversation that does not exist, byte
//!   for byte; restoring the conversation does not share it again. A member of another tenant
//!   never sees them. A copy of the conversation carries no dashboard.
//!
//! `tests/dashboards_api.rs` tests the drafts of a conversation that is not shared: another
//! member of the tenant gets the 404 there.
//!
//! Starts a throwaway Postgres with TLS (podman or docker); skipped without either unless `CI`
//! is set.

mod dashboard_world;

use std::sync::Arc;

use assistant::dashboards::caps::RunCaps;
use axum::http::Method;
use contract::{DashboardId, ServiceName, TenantGrant, TenantId, TenantRole};
use dashboard_world::{Answer, Content, Peers, Person, World, call, world};
use serde_json::{Value, json};
use uuid::Uuid;

const CONVERSATIONS: &str = "/api/assistant/v1/conversations";
const DASHBOARDS: &str = "/api/assistant/v1/dashboards";

fn dashboard(id: DashboardId) -> String {
    format!("{DASHBOARDS}/{id}")
}

fn action(id: DashboardId, action: &str) -> String {
    format!("{DASHBOARDS}/{id}/{action}")
}

fn drafts(conversation: Uuid) -> String {
    format!("{CONVERSATIONS}/{conversation}/dashboards")
}

fn share(conversation: Uuid) -> String {
    format!("{CONVERSATIONS}/{conversation}/share")
}

fn run_body(version: u32, dataset: &str) -> Value {
    json!({"version": version, "dataset": dataset})
}

/// An M1 error with `status`, `code` and `key`, not stored.
fn assert_error(answer: &Answer, status: u16, code: &str, key: &str) {
    assert_eq!(answer.status, status, "{answer:?}");
    assert_eq!(
        answer.error(),
        (code.to_owned(), key.to_owned()),
        "{answer:?}"
    );
    assert_eq!(
        answer.header("cache-control"),
        Some("no-store"),
        "{answer:?}"
    );
}

/// A 200 that must not be stored; its JSON.
fn ok(answer: &Answer) -> Value {
    assert_eq!(answer.status, 200, "{answer:?}");
    assert_eq!(
        answer.header("cache-control"),
        Some("no-store"),
        "{answer:?}"
    );
    answer.json()
}

/// The world with the stand-in peers and the API in this process.
struct Setup {
    world: World,
    peers: Peers,
    api: dashboard_world::Api,
}

async fn setup() -> Option<Setup> {
    let world = world().await?;
    let peers = Peers::start(Arc::clone(&world.issuer)).await;
    let api = world.api(&peers, RunCaps::default()).await;
    Some(Setup { world, peers, api })
}

/// `owner`'s conversation with a draft of two versions of different content.
async fn conversation_with_draft(world: &World, owner: &Person) -> (Uuid, DashboardId) {
    let conversation = world.conversation(owner).await;
    let draft = world
        .create_draft(owner, conversation, &Content::new("Owner's first"))
        .await
        .dashboard_id;
    world
        .write_draft(
            owner,
            conversation,
            draft,
            1,
            None,
            &Content::new("Owner's second"),
        )
        .await
        .expect("version 2");
    (conversation, draft)
}

/// The owner's share (`PUT`) or unshare (`DELETE`) of the conversation through the API.
async fn set_shared(world: &World, base: &str, owner: &Person, conversation: Uuid, on: bool) {
    let method = if on { Method::PUT } else { Method::DELETE };
    let answer = call(
        base,
        method,
        &share(conversation),
        Some(&world.token(owner)),
        None,
    )
    .await;
    assert_eq!(answer.status, 200, "{answer:?}");
}

/// The dashboards and versions of `tenant`, as the database holds them.
async fn stored(world: &World, tenant: TenantId) -> (i64, i64) {
    let mut admin = world.superuser().await;
    sqlx::query_as(
        "SELECT
            (SELECT count(*) FROM assistant.dashboards WHERE tenant_id = $1::uuid),
            (SELECT count(*) FROM assistant.dashboard_versions WHERE tenant_id = $1::uuid)",
    )
    .bind(tenant.to_string())
    .fetch_one(&mut admin)
    .await
    .expect("count the dashboards")
}

/// Every route of a draft, with a valid body for each write.
fn routes(conversation: Uuid, draft: DashboardId) -> Vec<(Method, String, Option<Value>)> {
    vec![
        (Method::GET, drafts(conversation), None),
        (Method::GET, dashboard(draft), None),
        (Method::GET, action(draft, "versions"), None),
        (
            Method::POST,
            action(draft, "dataset-runs"),
            Some(run_body(2, "daily")),
        ),
        (
            Method::POST,
            action(draft, "publish"),
            Some(json!({"version": 2})),
        ),
        (
            Method::POST,
            action(draft, "restore"),
            Some(json!({"from_version": 1, "base_version": 2})),
        ),
        (Method::PUT, action(draft, "archive"), None),
        (Method::DELETE, action(draft, "archive"), None),
    ]
}

/// Every route of a draft answers `person` exactly as for a dashboard (or conversation) that
/// does not exist, and runs nothing.
async fn assert_hidden(
    world: &World,
    peers: &Peers,
    base: &str,
    person: &Person,
    conversation: Uuid,
    draft: DashboardId,
) {
    let token = world.token(person);
    let calls = peers.calls().len();
    let missing = routes(Uuid::now_v7(), DashboardId::generate());
    for ((method, path, body), (_, nothing, _)) in routes(conversation, draft).iter().zip(&missing)
    {
        let answer = call(base, method.clone(), path, Some(&token), body.as_ref()).await;
        let expected = call(base, method.clone(), nothing, Some(&token), body.as_ref()).await;
        // A viewer's publish and archive are refused from the token before any lookup: a 403
        // for every dashboard, which shows none.
        let token_refused = person.role() == TenantRole::Viewer
            && (path.ends_with("/publish") || path.ends_with("/archive"));
        if token_refused {
            assert_error(&answer, 403, "permission_denied", "error.permission_denied");
        } else {
            assert_error(&answer, 404, "not_found", "error.not_found");
        }
        assert_eq!(
            (answer.status, &answer.text, answer.header("cache-control")),
            (
                expected.status,
                &expected.text,
                expected.header("cache-control")
            ),
            "{method} {path}"
        );
        assert!(!answer.text.contains(&draft.to_string()), "{answer:?}");
    }
    assert_eq!(peers.calls().len(), calls, "no dataset ran");
}

/// Acceptance criterion 5 (Laysi/trustiot-v2#650 with Laysi/trustiot-v2#150): a reader of a shared
/// conversation, as a viewer, an editor and an admin of the tenant, lists and opens its drafts
/// and versions, and runs a dataset as themselves; every write is refused and nothing changes.
/// The owner's own view and rights are unchanged, and a reader's copy of the conversation carries
/// no dashboard.
#[tokio::test(flavor = "multi_thread")]
async fn readers_of_a_shared_conversation_see_its_drafts_read_only_and_run_them_as_themselves() {
    let Some(Setup { world, peers, api }) = setup().await else {
        return;
    };
    let base = api.url.as_str();
    let tenant = TenantId::generate();
    let owner = world.person(tenant, TenantRole::Editor);
    let (conversation, draft) = conversation_with_draft(&world, &owner).await;
    set_shared(&world, base, &owner, conversation, true).await;
    let before = stored(&world, tenant).await;
    assert_eq!(before, (1, 2));
    let nothing_allowed = json!({"can_publish": false, "can_restore": false, "can_archive": false});

    for role in [TenantRole::Viewer, TenantRole::Editor, TenantRole::Admin] {
        let reader = world.person(tenant, role);
        let token = world.token(&reader);

        // The conversation's drafts, the draft and its versions.
        let listed = ok(&call(base, Method::GET, &drafts(conversation), Some(&token), None).await);
        let ids: Vec<&Value> = listed["dashboards"]
            .as_array()
            .expect("a list")
            .iter()
            .map(|item| &item["dashboard_id"])
            .collect();
        assert_eq!(ids, [&json!(draft)], "{role:?}");
        let opened = ok(&call(base, Method::GET, &dashboard(draft), Some(&token), None).await);
        assert_eq!(opened["permissions"], nothing_allowed, "{role:?}");
        assert_eq!(opened["dashboard"]["kind"], "draft", "{opened}");
        let versions = ok(&call(
            base,
            Method::GET,
            &action(draft, "versions"),
            Some(&token),
            None,
        )
        .await);
        assert_eq!(
            versions["versions"].as_array().map(Vec::len),
            Some(2),
            "{versions}"
        );
        let first = ok(&call(
            base,
            Method::GET,
            &format!("{}?version=1", dashboard(draft)),
            Some(&token),
            None,
        )
        .await);
        assert_eq!(first["version"]["version"], 1, "{first}");

        // A dataset run: telemetry-query gets the reader's token, with the reader's tenant and
        // role, never the owner's.
        let calls = peers.calls().len();
        let exchanged = peers.exchanged().len();
        let run = ok(&call(
            base,
            Method::POST,
            &action(draft, "dataset-runs"),
            Some(&token),
            Some(&run_body(2, "daily")),
        )
        .await);
        assert_eq!(run["dataset"], "daily");
        let made = peers.calls();
        assert_eq!(made.len(), calls + 1, "one call for the run");
        let last = made.last().expect("the reader's run");
        assert_eq!(last.claims.sub, reader.claims.sub, "{last:?}");
        assert_ne!(last.claims.sub, owner.claims.sub);
        assert_eq!(last.claims.tenant, Some(TenantGrant { id: tenant, role }));
        assert_eq!(last.claims.aud, ServiceName::TelemetryQuery);
        let exchanges = peers.exchanged();
        assert!(exchanges.len() > exchanged, "the run exchanged a token");
        assert!(
            exchanges[exchanged..]
                .iter()
                .all(|claims| claims.sub == reader.claims.sub),
            "only the reader's identity was exchanged"
        );

        // Every write is refused. A viewer's publish and archive stop at the handler's role
        // check; an editor's or admin's publish and every reader's restore at the owner check;
        // an archive of a draft is refused to everyone, the owner included.
        let publish = call(
            base,
            Method::POST,
            &action(draft, "publish"),
            Some(&token),
            Some(&json!({"version": 2})),
        )
        .await;
        let restore = call(
            base,
            Method::POST,
            &action(draft, "restore"),
            Some(&token),
            Some(&json!({"from_version": 1, "base_version": 2})),
        )
        .await;
        assert_error(
            &restore,
            403,
            "permission_denied",
            "dashboard.owner_required",
        );
        for method in [Method::PUT, Method::DELETE] {
            let archive = call(base, method, &action(draft, "archive"), Some(&token), None).await;
            if role == TenantRole::Viewer {
                assert_error(
                    &archive,
                    403,
                    "permission_denied",
                    "error.permission_denied",
                );
            } else {
                assert_error(
                    &archive,
                    422,
                    "validation_failed",
                    "dashboard.saved_required",
                );
            }
        }
        if role == TenantRole::Viewer {
            assert_error(
                &publish,
                403,
                "permission_denied",
                "error.permission_denied",
            );
        } else {
            assert_error(
                &publish,
                403,
                "permission_denied",
                "dashboard.owner_required",
            );
        }
        assert_eq!(
            stored(&world, tenant).await,
            before,
            "{role:?}: nothing changed"
        );
    }

    // The owner still may publish and restore their draft.
    let own = ok(&call(
        base,
        Method::GET,
        &dashboard(draft),
        Some(&world.token(&owner)),
        None,
    )
    .await);
    assert_eq!(
        own["permissions"],
        json!({"can_publish": true, "can_restore": true, "can_archive": false})
    );

    // A reader's copy is a conversation of their own without a dashboard.
    let reader = world.person(tenant, TenantRole::Admin);
    let token = world.token(&reader);
    let copied = call(
        base,
        Method::POST,
        &format!("{CONVERSATIONS}/{conversation}/copy"),
        Some(&token),
        Some(&json!({})),
    )
    .await;
    assert_eq!(copied.status, 201, "{copied:?}");
    let copy: Uuid = copied.json()["id"]
        .as_str()
        .expect("an id")
        .parse()
        .expect("a UUID");
    let none = ok(&call(base, Method::GET, &drafts(copy), Some(&token), None).await);
    assert_eq!(none["dashboards"], json!([]), "{none}");
    assert_eq!(
        stored(&world, tenant).await,
        before,
        "the copy took no dashboard"
    );
}

/// Acceptance criterion 5 (Laysi/trustiot-v2#650 with Laysi/trustiot-v2#150): unsharing and
/// deleting take a shared conversation's drafts from its readers at once, every route answering as
/// for what does not exist, byte for byte; sharing again brings them back, restoring a deleted
/// conversation does not; a member of another tenant never sees them.
#[tokio::test(flavor = "multi_thread")]
async fn unsharing_and_deleting_hide_the_drafts_from_readers_as_if_they_did_not_exist() {
    let Some(Setup { world, peers, api }) = setup().await else {
        return;
    };
    let base = api.url.as_str();
    let tenant = TenantId::generate();
    let owner = world.person(tenant, TenantRole::Editor);
    let (conversation, draft) = conversation_with_draft(&world, &owner).await;
    let readers = [
        world.person(tenant, TenantRole::Viewer),
        world.person(tenant, TenantRole::Admin),
    ];
    let stranger = world.person(TenantId::generate(), TenantRole::Admin);
    let sees = |person: &Person| {
        let token = world.token(person);
        async move {
            call(base, Method::GET, &dashboard(draft), Some(&token), None)
                .await
                .status
        }
    };

    set_shared(&world, base, &owner, conversation, true).await;
    for reader in &readers {
        assert_eq!(sees(reader).await, 200);
    }
    assert_hidden(&world, &peers, base, &stranger, conversation, draft).await;

    // Unshared: gone for the readers, as if it did not exist.
    set_shared(&world, base, &owner, conversation, false).await;
    for reader in &readers {
        assert_hidden(&world, &peers, base, reader, conversation, draft).await;
    }
    assert_eq!(sees(&owner).await, 200, "the owner keeps the draft");

    // Shared again, then deleted: gone again, and restoring does not share it again.
    set_shared(&world, base, &owner, conversation, true).await;
    for reader in &readers {
        assert_eq!(sees(reader).await, 200);
    }
    let deleted = call(
        base,
        Method::DELETE,
        &format!("{CONVERSATIONS}/{conversation}"),
        Some(&world.token(&owner)),
        None,
    )
    .await;
    assert_eq!(deleted.status, 204, "{deleted:?}");
    for reader in &readers {
        assert_hidden(&world, &peers, base, reader, conversation, draft).await;
    }
    world.restore_conversation(&owner, conversation).await;
    assert_eq!(sees(&owner).await, 200, "the owner's draft came back");
    for reader in &readers {
        assert_hidden(&world, &peers, base, reader, conversation, draft).await;
    }
    assert_hidden(&world, &peers, base, &stranger, conversation, draft).await;
    assert_eq!(stored(&world, tenant).await, (1, 2), "nothing changed");
}
