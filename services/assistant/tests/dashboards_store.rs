//! Dashboards in Postgres (Laysi/trustiot-v2#150, `assistant::dashboards::store`): the product's
//! schema, applied by the migration Job itself (`testkit::product`), and the service role
//! `assistant_app` through service-kit's tenant transactions, as the API and the `dashboard`
//! tool use them.
//!
//! - Versions are only added, numbered without gaps; a write based on an older version, and
//!   publishing over a saved dashboard that changed since the draft's base, are refused.
//! - Publishing, restoring and archiving follow the owner's table of who may do what; a draft
//!   is seen by its conversation's owner only, and a dashboard the caller cannot see is the
//!   same `NotFound` as one that does not exist.
//! - The tool writes only into its own conversation's drafts.
//! - The service role cannot change or remove a version, nor change a dashboard beyond what the
//!   store does; purging a conversation removes its drafts and keeps what was published.
//!
//! Starts a throwaway Postgres with TLS (podman or docker); skipped without either unless `CI`
//! is set.

mod dashboard_world;

use assistant::dashboards::store::{self, Access, ListCursor, StoreError, VersionCursor};
use contract::api::dashboard::public::{DashboardKind, SavedBase, VersionOrigin};
use contract::{DashboardId, TenantId, TenantRole};
use dashboard_world::{Content, Person, World, finish, unlimited, world};
use sqlx::Row;
use uuid::Uuid;

/// SQLSTATE `insufficient_privilege`.
const INSUFFICIENT_PRIVILEGE: &str = "42501";
/// SQLSTATE `foreign_key_violation`.
const FOREIGN_KEY_VIOLATION: &str = "23503";
/// SQLSTATE `check_violation`.
const CHECK_VIOLATION: &str = "23514";

/// The SQLSTATE `sql` fails with when the service role runs it in a transaction of the
/// person's tenant with `binds` as `$1`, `$2`, ..., or `None` when it succeeds. The
/// transaction is rolled back.
async fn as_service_role(
    world: &World,
    person: &Person,
    sql: &'static str,
    binds: &[DashboardId],
) -> Option<String> {
    let mut tx = world.tx(person).await;
    let mut query = sqlx::query(sql);
    for bind in binds {
        query = query.bind(bind.to_string());
    }
    let outcome = query.execute(&mut *tx).await;
    tx.rollback().await.expect("roll back");
    outcome.err().map(|error| {
        error
            .as_database_error()
            .and_then(|database| database.code())
            .map(|code| code.into_owned())
            .unwrap_or_else(|| format!("not a database error: {error}"))
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn versions_are_added_and_old_ones_stay_readable() {
    let Some(world) = world().await else { return };
    let owner = world.person(TenantId::generate(), TenantRole::Viewer);
    let conversation = world.conversation(&owner).await;
    let first = Content::new("Energy");
    let draft = world.create_draft(&owner, conversation, &first).await;
    assert_eq!(draft.version, 1);
    let second = Content::new("Energy by day");
    let written = world
        .write_draft(&owner, conversation, draft.dashboard_id, 1, None, &second)
        .await
        .expect("the second version");
    assert_eq!(
        (written.dashboard_id, written.version),
        (draft.dashboard_id, 2)
    );

    // A write based on version 1 is refused now, and names the latest.
    let stale = world
        .write_draft(&owner, conversation, draft.dashboard_id, 1, None, &first)
        .await;
    assert!(
        matches!(stale, Err(StoreError::VersionConflict { latest: 2 })),
        "{stale:?}"
    );

    let mut tx = world.tx(&owner).await;
    let latest = store::open(&mut tx, owner.principal(), draft.dashboard_id, None)
        .await
        .expect("the latest version");
    let old = store::open(&mut tx, owner.principal(), draft.dashboard_id, Some(1))
        .await
        .expect("version 1");
    let missing = store::open(&mut tx, owner.principal(), draft.dashboard_id, Some(3)).await;
    let versions = store::versions(&mut tx, owner.principal(), draft.dashboard_id, None)
        .await
        .expect("the versions");
    tx.rollback().await.expect("roll back");

    assert_eq!(latest.access, Access::Owner);
    assert_eq!(latest.dashboard.kind, DashboardKind::Draft);
    assert_eq!(latest.dashboard.latest_version, 2);
    assert_eq!(latest.dashboard.title, "Energy by day");
    assert_eq!(latest.dashboard.conversation_id, Some(conversation));
    assert_eq!(latest.dashboard.created_by, owner.claims.sub);
    assert_eq!(latest.version.version, 2);
    assert_eq!(latest.version.page, second.page);
    assert_eq!(old.version.title, "Energy");
    assert_eq!(old.version.spec, first.spec);
    assert_eq!(old.version.page, first.page);
    assert_eq!(old.version.description, first.description);
    assert_eq!(old.version.origin, VersionOrigin::Agent);
    assert!(
        matches!(missing, Err(StoreError::VersionNotFound)),
        "{missing:?}"
    );
    let numbers: Vec<u32> = versions.versions.iter().map(|v| v.version).collect();
    assert_eq!(numbers, [2, 1]);
    assert_eq!(versions.next_cursor, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn publishing_adds_versions_to_the_saved_dashboard_and_refuses_when_it_changed() {
    let Some(world) = world().await else { return };
    let tenant = TenantId::generate();
    let alice = world.person(tenant, TenantRole::Editor);
    let bob = world.person(tenant, TenantRole::Admin);

    // Alice's first publish makes a saved dashboard whose version 1 is her draft's version.
    let content = Content::new("Plant energy");
    let (alice_conversation, alice_draft, saved) = world.saved(&alice, &content).await;
    let mut tx = world.tx(&bob).await;
    let opened = store::open(&mut tx, bob.principal(), saved, None)
        .await
        .expect("a member sees the saved dashboard");
    let draft = store::open(&mut tx, alice.principal(), alice_draft, None)
        .await
        .expect("Alice sees her draft");
    tx.rollback().await.expect("roll back");
    assert_eq!(opened.access, Access::Member);
    assert_eq!(opened.dashboard.kind, DashboardKind::Saved);
    assert_eq!(opened.dashboard.created_by, alice.claims.sub);
    assert_eq!(opened.dashboard.conversation_id, None);
    assert_eq!(opened.version.origin, VersionOrigin::Publish);
    assert_eq!(opened.version.created_by, alice.claims.sub);
    assert_eq!(opened.version.spec, content.spec);
    assert_eq!(opened.version.page, content.page);
    assert_eq!(
        draft.dashboard.saved,
        Some(SavedBase {
            dashboard_id: saved,
            version: Some(1)
        })
    );

    // Alice's next publish adds version 2 and rebases her draft on it.
    world
        .write_draft(
            &alice,
            alice_conversation,
            alice_draft,
            1,
            None,
            &Content::new("Plant energy 2"),
        )
        .await
        .expect("Alice's version 2");
    let second = world
        .publish(&alice, alice_draft, 2)
        .await
        .expect("publish version 2");
    assert_eq!((second.dashboard_id, second.version), (saved, 2));

    // Bob takes a draft of version 2 into his own conversation.
    let bob_conversation = world.conversation(&bob).await;
    let bob_draft = world
        .fork(
            &bob,
            bob_conversation,
            saved,
            2,
            &Content::new("Bob's plant energy"),
        )
        .await
        .expect("Bob's draft");
    // Alice publishes version 3 meanwhile.
    world
        .write_draft(
            &alice,
            alice_conversation,
            alice_draft,
            2,
            None,
            &Content::new("Plant energy 3"),
        )
        .await
        .expect("Alice's version 3");
    world
        .publish(&alice, alice_draft, 3)
        .await
        .expect("publish version 3");

    // Bob's draft is based on version 2: publishing it would overwrite Alice's version 3.
    let refused = world.publish(&bob, bob_draft.dashboard_id, 1).await;
    assert!(
        matches!(refused, Err(StoreError::SavedChanged { latest: 3 })),
        "{refused:?}"
    );
    // A rebase only moves forward, within the saved dashboard's versions; a draft without a
    // saved dashboard has nothing to rebase on.
    for bad in [1, 4] {
        let rebased = world
            .write_draft(
                &bob,
                bob_conversation,
                bob_draft.dashboard_id,
                1,
                Some(bad),
                &Content::new("x"),
            )
            .await;
        assert!(
            matches!(rebased, Err(StoreError::InvalidRebase)),
            "{bad}: {rebased:?}"
        );
    }
    let unsaved = world
        .create_draft(&bob, bob_conversation, &Content::new("Unsaved"))
        .await;
    let rebased = world
        .write_draft(
            &bob,
            bob_conversation,
            unsaved.dashboard_id,
            1,
            Some(1),
            &Content::new("x"),
        )
        .await;
    assert!(
        matches!(rebased, Err(StoreError::InvalidRebase)),
        "{rebased:?}"
    );
    // Once the question merged version 3 in, the draft is based on it and publishes.
    world
        .write_draft(
            &bob,
            bob_conversation,
            bob_draft.dashboard_id,
            1,
            Some(3),
            &Content::new("Merged"),
        )
        .await
        .expect("the merged version");
    let merged = world
        .publish(&bob, bob_draft.dashboard_id, 2)
        .await
        .expect("publish the merged version");
    assert_eq!((merged.dashboard_id, merged.version), (saved, 4));

    // Archived: nothing is published into it, no draft is taken from it, nothing is restored.
    let mut tx = world.tx(&bob).await;
    let archived = store::set_archived(&mut tx, bob.requester(), saved, true).await;
    let archived = finish(tx, archived).await.expect("archive");
    let archived_at = archived.archived_at.expect("archived");
    let mut tx = world.tx(&alice).await;
    let again = store::set_archived(&mut tx, alice.requester(), saved, true).await;
    let again = finish(tx, again).await.expect("archive again");
    assert_eq!(
        again.archived_at,
        Some(archived_at),
        "archiving again keeps the first time"
    );
    let refused = world.publish(&alice, alice_draft, 3).await;
    assert!(matches!(refused, Err(StoreError::Archived)), "{refused:?}");
    let other_conversation = world.conversation(&alice).await;
    let forked = world
        .fork(&alice, other_conversation, saved, 4, &Content::new("x"))
        .await;
    assert!(matches!(forked, Err(StoreError::Archived)), "{forked:?}");
    let mut tx = world.tx(&alice).await;
    let restored = store::restore(&mut tx, alice.requester(), saved, 1, 4, unlimited).await;
    tx.rollback().await.expect("roll back");
    assert!(
        matches!(restored, Err(StoreError::Archived)),
        "{restored:?}"
    );

    // Unarchived, it takes versions again; Alice's version 3 is still based on version 2.
    let mut tx = world.tx(&alice).await;
    let unarchived = store::set_archived(&mut tx, alice.requester(), saved, false).await;
    let unarchived = finish(tx, unarchived).await.expect("unarchive");
    assert_eq!(unarchived.archived_at, None);
    let refused = world.publish(&alice, alice_draft, 3).await;
    assert!(
        matches!(refused, Err(StoreError::SavedChanged { latest: 4 })),
        "{refused:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn who_may_publish_restore_and_archive() {
    let Some(world) = world().await else { return };
    let tenant = TenantId::generate();
    let editor = world.person(tenant, TenantRole::Editor);
    let viewer = world.person(tenant, TenantRole::Viewer);
    let admin = world.person(tenant, TenantRole::Admin);
    let (saved_conversation, saved_draft, saved) =
        world.saved(&editor, &Content::new("Saved")).await;
    let viewer_conversation = world.conversation(&viewer).await;
    let viewer_draft = world
        .create_draft(
            &viewer,
            viewer_conversation,
            &Content::new("Viewer's draft"),
        )
        .await
        .dashboard_id;

    // A viewer publishes nothing, and restores and archives no saved dashboard.
    let published = world.publish(&viewer, viewer_draft, 1).await;
    assert!(
        matches!(published, Err(StoreError::EditorRequired)),
        "{published:?}"
    );
    let mut tx = world.tx(&viewer).await;
    let restored = store::restore(&mut tx, viewer.requester(), saved, 1, 1, unlimited).await;
    let archived = store::set_archived(&mut tx, viewer.requester(), saved, true).await;
    let unarchived = store::set_archived(&mut tx, viewer.requester(), saved, false).await;
    tx.rollback().await.expect("roll back");
    assert!(
        matches!(restored, Err(StoreError::EditorRequired)),
        "{restored:?}"
    );
    assert!(
        matches!(archived, Err(StoreError::EditorRequired)),
        "{archived:?}"
    );
    assert!(
        matches!(unarchived, Err(StoreError::EditorRequired)),
        "{unarchived:?}"
    );

    // A viewer restores their own draft: a copy of version 1 as version 3.
    world
        .write_draft(
            &viewer,
            viewer_conversation,
            viewer_draft,
            1,
            None,
            &Content::new("Second"),
        )
        .await
        .expect("version 2");
    let mut tx = world.tx(&viewer).await;
    let stale = store::restore(&mut tx, viewer.requester(), viewer_draft, 1, 1, unlimited).await;
    tx.rollback().await.expect("roll back");
    assert!(
        matches!(stale, Err(StoreError::VersionConflict { latest: 2 })),
        "{stale:?}"
    );
    for missing in [0, 3] {
        let mut tx = world.tx(&viewer).await;
        let restored = store::restore(
            &mut tx,
            viewer.requester(),
            viewer_draft,
            missing,
            2,
            unlimited,
        )
        .await;
        tx.rollback().await.expect("roll back");
        assert!(
            matches!(restored, Err(StoreError::VersionNotFound)),
            "{missing}: {restored:?}"
        );
    }
    let mut tx = world.tx(&viewer).await;
    let restored = store::restore(&mut tx, viewer.requester(), viewer_draft, 1, 2, unlimited).await;
    let restored = finish(tx, restored).await.expect("restore version 1");
    assert_eq!(restored.version, 3);
    let mut tx = world.tx(&viewer).await;
    let copy = store::open(&mut tx, viewer.principal(), viewer_draft, Some(3)).await;
    let source = store::open(&mut tx, viewer.principal(), viewer_draft, Some(1)).await;
    tx.rollback().await.expect("roll back");
    let (copy, source) = (copy.expect("version 3"), source.expect("version 1"));
    assert_eq!(copy.version.origin, VersionOrigin::Restore);
    assert_eq!(copy.version.restored_from, Some(1));
    assert_eq!(copy.version.created_by, viewer.claims.sub);
    assert_eq!(
        (
            &copy.version.title,
            &copy.version.description,
            &copy.version.spec,
            &copy.version.page,
            copy.version.runtime
        ),
        (
            &source.version.title,
            &source.version.description,
            &source.version.spec,
            &source.version.page,
            source.version.runtime
        )
    );

    // Another member of the tenant, whatever their role, cannot see the draft: the same
    // NotFound as a dashboard that does not exist.
    for other in [&editor, &admin] {
        let mut tx = world.tx(other).await;
        let outcomes = [
            store::open(&mut tx, other.principal(), viewer_draft, None)
                .await
                .err(),
            store::versions(&mut tx, other.principal(), viewer_draft, None)
                .await
                .err(),
            store::dataset(&mut tx, other.principal(), viewer_draft, 1, "daily")
                .await
                .err(),
            store::list_drafts(&mut tx, other.principal(), viewer_conversation, None)
                .await
                .err(),
            store::restore(&mut tx, other.requester(), viewer_draft, 1, 3, unlimited)
                .await
                .err(),
            store::publish(&mut tx, other.requester(), viewer_draft, 1, unlimited)
                .await
                .err(),
            store::set_archived(&mut tx, other.requester(), viewer_draft, true)
                .await
                .err(),
        ];
        tx.rollback().await.expect("roll back");
        for outcome in outcomes {
            assert!(matches!(outcome, Some(StoreError::NotFound)), "{outcome:?}");
        }
    }
    let mut tx = world.tx(&editor).await;
    let nothing = store::open(&mut tx, editor.principal(), DashboardId::generate(), None).await;
    tx.rollback().await.expect("roll back");
    assert!(matches!(nothing, Err(StoreError::NotFound)), "{nothing:?}");

    // The kinds: a saved dashboard is not published, a draft is not archived.
    let mut tx = world.tx(&admin).await;
    let published = store::publish(&mut tx, admin.requester(), saved, 1, unlimited).await;
    tx.rollback().await.expect("roll back");
    assert!(
        matches!(published, Err(StoreError::DraftRequired)),
        "{published:?}"
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
    let mut tx = world.tx(&editor).await;
    let archived = store::set_archived(&mut tx, editor.requester(), editor_draft, true).await;
    tx.rollback().await.expect("roll back");
    assert!(
        matches!(archived, Err(StoreError::SavedRequired)),
        "{archived:?}"
    );

    // An editor restores a saved dashboard; an admin archives it.
    world
        .write_draft(
            &editor,
            saved_conversation,
            saved_draft,
            1,
            None,
            &Content::new("Saved 2"),
        )
        .await
        .expect("version 2 of the editor's draft");
    world
        .publish(&editor, saved_draft, 2)
        .await
        .expect("publish version 2");
    let mut tx = world.tx(&editor).await;
    let restored = store::restore(&mut tx, editor.requester(), saved, 1, 2, unlimited).await;
    let restored = finish(tx, restored)
        .await
        .expect("restore the saved dashboard");
    assert_eq!((restored.dashboard_id, restored.version), (saved, 3));
    let mut tx = world.tx(&admin).await;
    let archived = store::set_archived(&mut tx, admin.requester(), saved, true).await;
    finish(tx, archived).await.expect("archive");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_tool_writes_only_its_own_conversations_drafts() {
    let Some(world) = world().await else { return };
    let tenant = TenantId::generate();
    let owner = world.person(tenant, TenantRole::Editor);
    let other = world.person(tenant, TenantRole::Admin);
    let conversation = world.conversation(&owner).await;
    let second_conversation = world.conversation(&owner).await;
    let others_conversation = world.conversation(&other).await;
    let content = Content::new("Draft");

    // Not into another user's conversation, nor one that does not exist.
    for target in [others_conversation, Uuid::now_v7()] {
        let mut tx = world.tx(&owner).await;
        let created =
            store::create_draft(&mut tx, owner.author(), target, &content.version()).await;
        tx.rollback().await.expect("roll back");
        assert!(matches!(created, Err(StoreError::NotFound)), "{created:?}");
    }
    let draft = world
        .create_draft(&owner, conversation, &content)
        .await
        .dashboard_id;
    let others_draft = world
        .create_draft(&other, others_conversation, &content)
        .await
        .dashboard_id;
    // Not into the same user's other conversation, nor another user's draft.
    let elsewhere = world
        .write_draft(&owner, second_conversation, draft, 1, None, &content)
        .await;
    assert!(
        matches!(elsewhere, Err(StoreError::NotFound)),
        "{elsewhere:?}"
    );
    let others = world
        .write_draft(&owner, others_conversation, others_draft, 1, None, &content)
        .await;
    assert!(matches!(others, Err(StoreError::NotFound)), "{others:?}");

    // A saved dashboard is not written into: the tool takes a draft of it, once per
    // conversation, from a version it has.
    let (_, _, saved) = world.saved(&other, &content).await;
    let into_saved = world
        .write_draft(&owner, conversation, saved, 1, None, &content)
        .await;
    assert!(
        matches!(into_saved, Err(StoreError::DraftRequired)),
        "{into_saved:?}"
    );
    for missing in [0, 2] {
        let forked = world
            .fork(&owner, conversation, saved, missing, &content)
            .await;
        assert!(
            matches!(forked, Err(StoreError::VersionNotFound)),
            "{missing}: {forked:?}"
        );
    }
    let forked = world
        .fork(&owner, conversation, saved, 1, &content)
        .await
        .expect("a draft of the saved dashboard");
    let again = world.fork(&owner, conversation, saved, 1, &content).await;
    match again {
        Err(StoreError::DraftExists { draft_id }) => assert_eq!(draft_id, forked.dashboard_id),
        other => panic!("expected DraftExists, got {other:?}"),
    }
    let of_a_draft = world
        .fork(&owner, second_conversation, draft, 1, &content)
        .await;
    assert!(
        matches!(of_a_draft, Err(StoreError::SavedRequired)),
        "{of_a_draft:?}"
    );
    // Another tenant's saved dashboard does not exist here.
    let stranger = world.person(TenantId::generate(), TenantRole::Admin);
    let (_, _, foreign) = world.saved(&stranger, &content).await;
    let foreign = world
        .fork(&owner, second_conversation, foreign, 1, &content)
        .await;
    assert!(matches!(foreign, Err(StoreError::NotFound)), "{foreign:?}");

    // Content that breaks the rules, or that Postgres text cannot hold, is not stored.
    let mut empty = content.clone();
    empty.page = "  ".into();
    let refused = world
        .write_draft(&owner, conversation, draft, 1, None, &empty)
        .await;
    assert!(
        matches!(refused, Err(StoreError::Invalid(_))),
        "{refused:?}"
    );
    let mut nul = content.clone();
    nul.page = "<p>\u{0}</p>".into();
    let refused = world
        .write_draft(&owner, conversation, draft, 1, None, &nul)
        .await;
    assert!(
        matches!(refused, Err(StoreError::Unstorable(_))),
        "{refused:?}"
    );

    // A transaction of another tenant than the author's is refused before anything is read.
    let mut tx = world.tx(&stranger).await;
    let wrong =
        store::create_draft(&mut tx, owner.author(), conversation, &content.version()).await;
    tx.rollback().await.expect("roll back");
    assert!(matches!(wrong, Err(StoreError::WrongTenant)), "{wrong:?}");

    // The tool's version carries its question's run.
    let author = owner.author();
    let mut tx = world.tx(&owner).await;
    let written = store::write_draft(
        &mut tx,
        author,
        conversation,
        draft,
        1,
        None,
        &content.version(),
    )
    .await;
    let written = finish(tx, written).await.expect("version 2");
    let mut admin = world.superuser().await;
    let run_id: String = sqlx::query(
        "SELECT run_id::text AS run_id FROM assistant.dashboard_versions
        WHERE dashboard_id = $1::uuid AND version = $2",
    )
    .bind(draft.to_string())
    .bind(i32::try_from(written.version).expect("small"))
    .fetch_one(&mut admin)
    .await
    .expect("the version")
    .get("run_id");
    assert_eq!(run_id, author.run_id.to_string());
}

#[tokio::test(flavor = "multi_thread")]
async fn lists_page_and_show_only_what_the_reader_may_see() {
    let Some(world) = world().await else { return };
    let tenant = TenantId::generate();
    let editor = world.person(tenant, TenantRole::Editor);
    let viewer = world.person(tenant, TenantRole::Viewer);
    let (_, _, kept) = world.saved(&editor, &Content::new("Kept")).await;
    let (_, _, archived) = world.saved(&editor, &Content::new("Archived")).await;
    let mut tx = world.tx(&editor).await;
    let changed = store::set_archived(&mut tx, editor.requester(), archived, true).await;
    finish(tx, changed).await.expect("archive");
    // Another tenant's saved dashboard.
    let stranger = world.person(TenantId::generate(), TenantRole::Admin);
    world.saved(&stranger, &Content::new("Foreign")).await;

    let mut tx = world.tx(&viewer).await;
    let listed = store::list_saved(&mut tx, viewer.principal(), false, None)
        .await
        .expect("the saved dashboards");
    let with_archived = store::list_saved(&mut tx, viewer.principal(), true, None)
        .await
        .expect("with the archived ones");
    tx.rollback().await.expect("roll back");
    let ids = |list: &contract::api::dashboard::public::DashboardList| {
        list.dashboards
            .iter()
            .map(|d| d.dashboard_id)
            .collect::<Vec<_>>()
    };
    // The most recently updated first; the drafts the editor published from are not saved
    // dashboards, so they never appear.
    assert_eq!(ids(&listed), [kept]);
    assert_eq!(ids(&with_archived), [archived, kept]);
    assert!(with_archived.dashboards[0].archived_at.is_some());

    // 51 drafts in one conversation: a page of 50 and one more.
    let conversation = world.conversation(&viewer).await;
    let mut drafts = Vec::new();
    for index in 0..=store::PAGE_SIZE {
        let created = world
            .create_draft(
                &viewer,
                conversation,
                &Content::new(&format!("Draft {index}")),
            )
            .await;
        drafts.push(created.dashboard_id);
    }
    drafts.reverse();
    let mut tx = world.tx(&viewer).await;
    let first = store::list_drafts(&mut tx, viewer.principal(), conversation, None)
        .await
        .expect("the first page");
    let cursor = ListCursor::parse(first.next_cursor.as_deref().expect("more"))
        .expect("the cursor the store wrote");
    let second = store::list_drafts(&mut tx, viewer.principal(), conversation, Some(cursor))
        .await
        .expect("the second page");
    let editor_sees = store::list_drafts(&mut tx, editor.principal(), conversation, None).await;
    tx.rollback().await.expect("roll back");
    let page = usize::try_from(store::PAGE_SIZE).expect("fits");
    assert_eq!(ids(&first), drafts.get(..page).expect("a page"));
    assert_eq!(ids(&second), drafts.get(page..).expect("the rest"));
    assert_eq!(second.next_cursor, None);
    assert!(
        matches!(editor_sees, Err(StoreError::NotFound)),
        "{editor_sees:?}"
    );

    // 51 versions of one draft: a page of 50, newest first, then version 1.
    let draft = drafts[0];
    for base in 1..=store::PAGE_SIZE {
        world
            .write_draft(
                &viewer,
                conversation,
                draft,
                base,
                None,
                &Content::new("Again"),
            )
            .await
            .expect("the next version");
    }
    let mut tx = world.tx(&viewer).await;
    let first = store::versions(&mut tx, viewer.principal(), draft, None)
        .await
        .expect("the first page");
    let cursor =
        VersionCursor::parse(first.next_cursor.as_deref().expect("more")).expect("a cursor");
    let second = store::versions(&mut tx, viewer.principal(), draft, Some(cursor))
        .await
        .expect("the second page");
    tx.rollback().await.expect("roll back");
    let numbers = |list: &contract::api::dashboard::public::VersionList| {
        list.versions.iter().map(|v| v.version).collect::<Vec<_>>()
    };
    assert_eq!(
        numbers(&first),
        (2..=store::PAGE_SIZE + 1).rev().collect::<Vec<_>>()
    );
    assert_eq!(numbers(&second), [1]);
    assert_eq!(second.next_cursor, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_service_role_cannot_change_what_was_written() {
    let Some(world) = world().await else { return };
    let tenant = TenantId::generate();
    let editor = world.person(tenant, TenantRole::Editor);
    let (conversation, draft, saved) = world.saved(&editor, &Content::new("Saved")).await;
    // The draft is based on version 2 of the saved dashboard.
    world
        .write_draft(&editor, conversation, draft, 1, None, &Content::new("Two"))
        .await
        .expect("version 2");
    world
        .publish(&editor, draft, 2)
        .await
        .expect("publish version 2");
    let (_, _, other_saved) = world.saved(&editor, &Content::new("Other")).await;

    let refusals: [(&'static str, Vec<DashboardId>); 9] = [
        // No UPDATE of a version (its base included), no DELETE or TRUNCATE of either table.
        (
            "UPDATE assistant.dashboard_versions SET title = 'x' WHERE dashboard_id = $1::uuid",
            vec![saved],
        ),
        (
            "UPDATE assistant.dashboard_versions SET base_saved_version = 2 WHERE dashboard_id = $1::uuid",
            vec![draft],
        ),
        (
            "DELETE FROM assistant.dashboard_versions WHERE dashboard_id = $1::uuid",
            vec![saved],
        ),
        (
            "DELETE FROM assistant.dashboards WHERE dashboard_id = $1::uuid",
            vec![draft],
        ),
        (
            "TRUNCATE assistant.dashboards, assistant.dashboard_versions CASCADE",
            vec![],
        ),
        // No column of a dashboard beyond the three the store changes.
        (
            "UPDATE assistant.dashboards SET kind = 'saved' WHERE dashboard_id = $1::uuid",
            vec![draft],
        ),
        (
            "UPDATE assistant.dashboards SET created_by = tenant_id WHERE dashboard_id = $1::uuid",
            vec![draft],
        ),
        // The trigger: a draft's saved dashboard is set once, and with it its base is never
        // cleared.
        (
            "UPDATE assistant.dashboards SET saved_dashboard_id = $2::uuid WHERE dashboard_id = $1::uuid",
            vec![draft, other_saved],
        ),
        (
            "UPDATE assistant.dashboards SET saved_dashboard_id = NULL, base_saved_version = NULL WHERE dashboard_id = $1::uuid",
            vec![draft],
        ),
    ];
    for (sql, binds) in &refusals {
        assert_eq!(
            as_service_role(&world, &editor, sql, binds)
                .await
                .as_deref(),
            Some(INSUFFICIENT_PRIVILEGE),
            "{sql}"
        );
    }
    // A draft's base may go down or become none (a restore brings back an older version's
    // base): publishing compares the base of the version it publishes, which never changes. A
    // draft without a saved dashboard has no base.
    for sql in [
        "UPDATE assistant.dashboards SET base_saved_version = 1 WHERE dashboard_id = $1::uuid",
        "UPDATE assistant.dashboards SET base_saved_version = NULL WHERE dashboard_id = $1::uuid",
    ] {
        assert_eq!(
            as_service_role(&world, &editor, sql, &[draft]).await,
            None,
            "{sql}"
        );
    }
    let unsaved = world
        .create_draft(&editor, conversation, &Content::new("Unsaved"))
        .await
        .dashboard_id;
    assert_eq!(
        as_service_role(
            &world,
            &editor,
            "UPDATE assistant.dashboards SET base_saved_version = 1 WHERE dashboard_id = $1::uuid",
            &[unsaved],
        )
        .await
        .as_deref(),
        Some(CHECK_VIOLATION)
    );
    // A published version is a saved dashboard's and has no base of its own.
    let published_with_a_base = "INSERT INTO assistant.dashboard_versions
        (tenant_id, dashboard_id, version, origin, runtime, title, spec, page, created_by,
         base_saved_version)
    SELECT tenant_id, dashboard_id, 3, 'publish', 1, 'Three', '{}', '<p></p>', created_by, 2
    FROM assistant.dashboards WHERE dashboard_id = $1::uuid";
    assert_eq!(
        as_service_role(&world, &editor, published_with_a_base, &[saved])
            .await
            .as_deref(),
        Some(CHECK_VIOLATION)
    );
    // The archive time is the database's clock: a time the role writes is replaced.
    let archived = "UPDATE assistant.dashboards SET archived_at = '2000-01-01T00:00:00Z'
        WHERE dashboard_id = $1::uuid RETURNING archived_at";
    let mut tx = world.tx(&editor).await;
    let archived_at: chrono::DateTime<chrono::Utc> = sqlx::query(archived)
        .bind(saved.to_string())
        .fetch_one(&mut *tx)
        .await
        .expect("archive by hand")
        .get("archived_at");
    tx.rollback().await.expect("roll back");
    assert!(
        archived_at > chrono::Utc::now() - chrono::TimeDelta::days(1),
        "{archived_at}"
    );
    // A version after a gap has no previous version.
    let gap = "INSERT INTO assistant.dashboard_versions
        (tenant_id, dashboard_id, version, origin, runtime, title, spec, page, created_by)
    SELECT tenant_id, dashboard_id, 5, 'publish', 1, 'Gap', '{}', '<p></p>', created_by
    FROM assistant.dashboards WHERE dashboard_id = $1::uuid";
    assert_eq!(
        as_service_role(&world, &editor, gap, &[saved])
            .await
            .as_deref(),
        Some(FOREIGN_KEY_VIOLATION)
    );
    // A draft of a draft is not a draft of a saved dashboard.
    let pointing_at_a_draft = "INSERT INTO assistant.dashboards
        (tenant_id, dashboard_id, kind, conversation_id, saved_dashboard_id, base_saved_version, created_by)
    SELECT tenant_id, gen_random_uuid(), 'draft', conversation_id, dashboard_id, 1, created_by
    FROM assistant.dashboards WHERE dashboard_id = $1::uuid";
    assert_eq!(
        as_service_role(&world, &editor, pointing_at_a_draft, &[draft])
            .await
            .as_deref(),
        Some(FOREIGN_KEY_VIOLATION)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn purging_a_conversation_removes_its_drafts_and_keeps_what_was_published() {
    let Some(world) = world().await else { return };
    let tenant = TenantId::generate();
    let editor = world.person(tenant, TenantRole::Editor);
    let viewer = world.person(tenant, TenantRole::Viewer);
    let (conversation, draft, saved) = world.saved(&editor, &Content::new("Saved")).await;
    let other_draft = world
        .create_draft(&editor, conversation, &Content::new("Another draft"))
        .await
        .dashboard_id;

    let mut admin = world.superuser().await;
    sqlx::query("DELETE FROM assistant.conversations WHERE conversation_id = $1::uuid")
        .bind(conversation.to_string())
        .execute(&mut admin)
        .await
        .expect("purge the conversation");
    let left: i64 = sqlx::query(
        "SELECT count(*) AS left FROM assistant.dashboard_versions
        WHERE dashboard_id = ANY($1::uuid[])",
    )
    .bind(vec![draft.to_string(), other_draft.to_string()])
    .fetch_one(&mut admin)
    .await
    .expect("count")
    .get("left");
    assert_eq!(left, 0, "the drafts' versions went with them");

    let mut tx = world.tx(&viewer).await;
    let opened = store::open(&mut tx, viewer.principal(), saved, None).await;
    let gone = store::open(&mut tx, editor.principal(), draft, None).await;
    tx.rollback().await.expect("roll back");
    let opened = opened.expect("the saved dashboard stays");
    assert_eq!(opened.version.title, "Saved");
    assert!(matches!(gone, Err(StoreError::NotFound)), "{gone:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_writes_of_one_draft_queue_and_the_later_one_conflicts() {
    let Some(world) = world().await else { return };
    let owner = world.person(TenantId::generate(), TenantRole::Editor);
    let conversation = world.conversation(&owner).await;
    let draft = world
        .create_draft(&owner, conversation, &Content::new("Draft"))
        .await
        .dashboard_id;

    // The first write holds the draft's row until it commits; the second, based on the same
    // version, waits for it and then finds a newer version, in whichever order they ran.
    let content = Content::new("Next");
    let mut first = world.tx(&owner).await;
    let written = store::write_draft(
        &mut first,
        owner.author(),
        conversation,
        draft,
        1,
        None,
        &content.version(),
    )
    .await
    .expect("the first write");
    let mut second = world.tx(&owner).await;
    let author = owner.author();
    let racing = tokio::spawn(async move {
        let content = Content::new("Racing");
        let written = store::write_draft(
            &mut second,
            author,
            conversation,
            draft,
            1,
            None,
            &content.version(),
        )
        .await;
        finish(second, written).await
    });
    first.commit().await.expect("commit the first");
    let racing = tokio::time::timeout(dashboard_world::WAIT, racing)
        .await
        .expect("the second write ends once the first committed")
        .expect("the task");
    assert_eq!(written.version, 2);
    assert!(
        matches!(racing, Err(StoreError::VersionConflict { latest: 2 })),
        "{racing:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs assistant.conversations.deleted_at (Laysi/trustiot-v2#644): un-ignore when it lands"]
async fn a_soft_deleted_conversations_drafts_are_hidden_until_it_is_restored() {
    let Some(world) = world().await else { return };
    let tenant = TenantId::generate();
    let owner = world.person(tenant, TenantRole::Editor);
    let member = world.person(tenant, TenantRole::Viewer);
    let (conversation, draft, saved) = world.saved(&owner, &Content::new("Saved")).await;

    let set_deleted = |deleted: bool| {
        let world = &world;
        async move {
            let mut admin = world.superuser().await;
            sqlx::query(
                "UPDATE assistant.conversations
                SET deleted_at = CASE WHEN $2 THEN now() END
                WHERE conversation_id = $1::uuid",
            )
            .bind(conversation.to_string())
            .bind(deleted)
            .execute(&mut admin)
            .await
            .expect("mark the conversation");
        }
    };
    set_deleted(true).await;
    let mut tx = world.tx(&owner).await;
    let hidden = store::open(&mut tx, owner.principal(), draft, None).await;
    let listed = store::list_drafts(&mut tx, owner.principal(), conversation, None).await;
    let written = store::write_draft(
        &mut tx,
        owner.author(),
        conversation,
        draft,
        1,
        None,
        &Content::new("x").version(),
    )
    .await;
    tx.rollback().await.expect("roll back");
    assert!(matches!(hidden, Err(StoreError::NotFound)), "{hidden:?}");
    assert!(matches!(listed, Err(StoreError::NotFound)), "{listed:?}");
    assert!(matches!(written, Err(StoreError::NotFound)), "{written:?}");
    // What was published stays.
    let mut tx = world.tx(&member).await;
    let opened = store::open(&mut tx, member.principal(), saved, None).await;
    tx.rollback().await.expect("roll back");
    opened.expect("the saved dashboard stays");

    set_deleted(false).await;
    let mut tx = world.tx(&owner).await;
    let back = store::open(&mut tx, owner.principal(), draft, None).await;
    tx.rollback().await.expect("roll back");
    back.expect("the draft is back");
}
