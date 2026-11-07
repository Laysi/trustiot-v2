//! 0004_dashboard_pages_optional.sql (Laysi/trustiot-v2#934) on a database the previous release
//! filled: the migration Job applies the product with the assistant's files up to
//! 0003_dashboards.sql (and the later files this build's store reads, `previous_release`), the
//! store writes drafts, published and restored versions (each with its
//! page, as that release wrote them), and 0003 refuses a version without a page; then the Job
//! runs again with every file, as the next release's does, and
//!
//! - every row of `assistant.dashboards` and `assistant.dashboard_versions` is unchanged, and
//!   every version opens through the store as before;
//! - 0003's content and size checks are gone, replaced by the two named checks of 0004, and no
//!   other check changed;
//! - the store writes a version of components only, which opens with neither page nor runtime;
//! - the database refuses a page without a runtime, a runtime without a page, and a spec over
//!   56 KiB without a page (which 0003's size check, NULL without a page, would have let pass);
//! - a third run of the Job applies nothing.
//!
//! 0010_dashboard_publications.sql (Laysi/trustiot-v2#1096, acceptance criterion 4) on a database
//! the previous release filled, in the same way: drafts written by the store and published as that
//! release published (a copy of the version, without a record of its source); then the Job runs
//! with every file, and
//!
//! - every row of `assistant.dashboards` and `assistant.dashboard_versions` is unchanged, every
//!   saved dashboard's version opens through the store as before, and every draft version opens
//!   with no `published_as` (its source is unknown, and no backfill guesses it);
//! - a publish from then on records its source, which opening the draft version shows;
//! - the service role cannot change or delete a record, and no role can change one.
//!
//! Starts a throwaway Postgres with TLS (podman or docker) of its own; skipped without either
//! unless `CI` is set.

mod dashboard_world;

use std::sync::{Arc, OnceLock};

use assistant::dashboards::store;
use contract::api::dashboard::public::{PublishedAs, VersionCreated};
use contract::{DashboardId, TenantId, TenantRole};
use dashboard_world::{Content, Person, World, finish, unlimited};
use migrate::config::{Platform, PostgresSettings, RunConfig, WaitSettings};
use migrate::registry::{PRODUCT, Registry, Service};
use serde_json::Value;
use service_kit::migrations::Migration;
use service_kit::postgres::SslMode;
use service_kit::tenant::TenantDb;
use sqlx::postgres::PgPoolOptions;
use sqlx::{Connection as _, PgConnection};
use testkit::identity::TestIssuer;
use testkit::postgres::{SERVER_HOST, TEST_PASSWORD, TlsPostgres, tls_postgres};
use testkit::product::{DATABASE, MIGRATOR, PRODUCT_INIT, ProductPostgres};

/// The assistant's files the previous release applied: up to 0003_dashboards.sql.
const PREVIOUS_FILES: usize = 3;

/// The file under test, the one the next release's Job adds.
const UNDER_TEST: &str = "0004_dashboard_pages_optional.sql";

/// The spec and the page together, at most (0003 and 0004).
const MAX_CONTENT_BYTES: usize = 57_344;

const CREATE_DATABASE: &str = "CREATE DATABASE trustiot OWNER trustiot_owner";

/// The Job leaves `assistant_app` NOLOGIN without a bound Secret; the chart's Secret would give
/// it a password, as here.
const ALLOW_LOGIN: &str = "ALTER ROLE assistant_app LOGIN PASSWORD 'testkit-postgres'";

/// Every row of both tables, as the superuser (no row-level security) reads them.
const DASHBOARDS: &str = "SELECT coalesce(jsonb_agg(to_jsonb(d) ORDER BY tenant_id, dashboard_id),
    '[]')::text FROM assistant.dashboards d";
const VERSIONS: &str = "SELECT coalesce(jsonb_agg(to_jsonb(v)
    ORDER BY tenant_id, dashboard_id, version), '[]')::text FROM assistant.dashboard_versions v";

/// The table's checks, by name.
const CHECKS: &str =
    "SELECT coalesce(jsonb_object_agg(conname, pg_get_constraintdef(oid)), '{}')::text
    FROM pg_catalog.pg_constraint
    WHERE conrelid = 'assistant.dashboard_versions'::regclass AND contype = 'c'";

/// The files the Job applied in the assistant's schema, by name.
const APPLIED: &str = "SELECT coalesce(jsonb_agg(name ORDER BY name), '[]')::text
    FROM assistant.trustiot_migrate_applied";

/// A version after `version` of the dashboard, copied from it with a runtime, a page and a spec
/// of the test's (the spec only when given), as the agent's: what the store would insert.
const NEXT_VERSION: &str = "INSERT INTO assistant.dashboard_versions
    (tenant_id, dashboard_id, version, origin, runtime, title, description, spec, page,
     created_by, run_id)
    SELECT tenant_id, dashboard_id, version + 1, 'agent', $4::smallint, title, description,
        coalesce($5::text, spec), $6::text, created_by, gen_random_uuid()
    FROM assistant.dashboard_versions
    WHERE tenant_id = $1::uuid AND dashboard_id = $2::uuid AND version = $3";

/// The registry of the previous release: the product with the assistant's first files, and the
/// files after [`UNDER_TEST`]. The store of this build writes what that release wrote, and uses
/// what those later files add (`0005_conversation_sharing.sql`'s `shared_at`, which decides who
/// sees a draft, Laysi/trustiot-v2#650; `0006_answer_removed_content.sql`'s `removed`, which
/// every message is written with, Laysi/trustiot-v2#646; `0007_answer_drafts.sql`'s
/// `answer_seq`, which every answered run is written with, Laysi/trustiot-v2#1043, and
/// `0008_answer_drafts_index.sql`'s index on it, which a purge's cascade reads;
/// `0009_answer_partials.sql`'s partial answers, which every run's end deletes,
/// Laysi/trustiot-v2#147; `0010_dashboard_publications.sql`'s record of each publish's source,
/// which every publish writes and opening a draft's version reads, Laysi/trustiot-v2#1096, and
/// which adds a table without changing the dashboards' tables); none of them
/// touches the dashboards' tables, so the next run of the Job adds [`UNDER_TEST`] alone, as the
/// next release's does.
fn previous_release() -> Registry {
    static MIGRATIONS: OnceLock<Vec<Migration>> = OnceLock::new();
    static SERVICES: OnceLock<Vec<Service>> = OnceLock::new();
    let services = SERVICES.get_or_init(|| {
        PRODUCT
            .services
            .iter()
            .map(|service| {
                let mut service = *service;
                if service.name == "assistant" {
                    let all = service.migrations;
                    let under_test = all
                        .iter()
                        .position(|migration| migration.name == UNDER_TEST)
                        .expect("the file under test");
                    assert_eq!(under_test, PREVIOUS_FILES, "{UNDER_TEST} follows 0003");
                    service.migrations = MIGRATIONS.get_or_init(|| {
                        all.iter()
                            .enumerate()
                            .filter(|(index, _)| *index != under_test)
                            .map(|(_, migration)| *migration)
                            .collect()
                    });
                }
                service
            })
            .collect()
    });
    Registry {
        services: services.as_slice(),
        ..PRODUCT
    }
}

fn run_config(postgres: &TlsPostgres, ca_file: &std::path::Path) -> RunConfig {
    RunConfig {
        platform: Platform {
            postgres_host: SERVER_HOST.to_owned(),
            postgres_port: postgres.address.port(),
            postgres_database: DATABASE.to_owned(),
            postgres_sslmode: SslMode::VerifyFull,
            postgres_ca_file: ca_file.to_owned(),
            clickhouse_url: None,
            clickhouse_ca_file: None,
        },
        postgres: PostgresSettings {
            username: MIGRATOR.to_owned(),
            password: TEST_PASSWORD.to_owned(),
        },
        clickhouse: None,
        passwords: None,
        wait: WaitSettings::default(),
    }
}

/// One run of the migration Job with `registry`, then LOGIN for the service role.
async fn migrate(product: &ProductPostgres, registry: &Registry, what: &str) {
    migrate::run(&run_config(&product.postgres, &product.ca_file), registry)
        .await
        .unwrap_or_else(|error| panic!("the migration Job's run ({what}): {error}"));
    let mut admin = product.superuser().await;
    sqlx::raw_sql(ALLOW_LOGIN)
        .execute(&mut admin)
        .await
        .expect("let assistant_app log in");
    admin.close().await.expect("close");
}

/// The JSON text `query` returns, read back.
async fn json(admin: &mut PgConnection, query: &'static str) -> Value {
    let text: String = sqlx::query_scalar(query)
        .fetch_one(&mut *admin)
        .await
        .unwrap_or_else(|error| panic!("{query}: {error}"));
    serde_json::from_str(&text).expect("JSON text")
}

/// What the database holds and what the store opens: both tables' rows, the checks, and every
/// version of `dashboards` as `reader` opens it.
#[derive(Debug, PartialEq)]
struct Snapshot {
    dashboards: Value,
    versions: Value,
    opened: Vec<store::Opened>,
}

async fn snapshot(world: &World, reader: &Person, dashboards: &[(DashboardId, u32)]) -> Snapshot {
    let mut admin = world.superuser().await;
    let rows = (
        json(&mut admin, DASHBOARDS).await,
        json(&mut admin, VERSIONS).await,
    );
    admin.close().await.expect("close");
    let mut opened = Vec::new();
    for &(dashboard, latest) in dashboards {
        for version in 1..=latest {
            let mut tx = world.tx(reader).await;
            let found = store::open(&mut tx, reader.principal(), dashboard, Some(version)).await;
            tx.rollback().await.expect("roll back");
            opened
                .push(found.unwrap_or_else(|error| panic!("open {dashboard} {version}: {error}")));
        }
    }
    Snapshot {
        dashboards: rows.0,
        versions: rows.1,
        opened,
    }
}

/// The next version of `dashboard` after `version` with a runtime, a spec and a page of the
/// test's, inserted as the superuser: the SQLSTATE and the check that refused it, if any.
async fn insert_next(
    world: &World,
    tenant: TenantId,
    dashboard: DashboardId,
    version: u32,
    runtime: Option<i16>,
    spec: Option<&str>,
    page: Option<&str>,
) -> Option<(String, String)> {
    let mut admin = world.superuser().await;
    let mut tx = admin.begin().await.expect("begin");
    let inserted = sqlx::query(NEXT_VERSION)
        .bind(tenant.to_string())
        .bind(dashboard.to_string())
        .bind(i32::try_from(version).expect("small"))
        .bind(runtime)
        .bind(spec)
        .bind(page)
        .execute(&mut *tx)
        .await;
    tx.rollback().await.expect("roll back");
    admin.close().await.expect("close");
    match inserted {
        Ok(done) => {
            assert_eq!(done.rows_affected(), 1, "the version to copy exists");
            None
        }
        Err(error) => {
            let database = error
                .as_database_error()
                .unwrap_or_else(|| panic!("not a database error: {error}"));
            Some((
                database.code().unwrap_or_default().into_owned(),
                database.constraint().unwrap_or_default().to_owned(),
            ))
        }
    }
}

/// A spec of `bytes` bytes of compact JSON text: one object with one string.
fn spec_of(bytes: usize) -> String {
    let filler = bytes
        .checked_sub(r#"{"a":""}"#.len())
        .expect("room for the object");
    format!(r#"{{"a":"{}"}}"#, "x".repeat(filler))
}

async fn publish(
    world: &World,
    person: &Person,
    draft: DashboardId,
    version: u32,
) -> VersionCreated {
    world
        .publish(person, draft, version)
        .await
        .unwrap_or_else(|error| panic!("publish {draft} {version}: {error:?}"))
}

async fn restore(
    world: &World,
    person: &Person,
    saved: DashboardId,
    from_version: u32,
    base_version: u32,
) -> VersionCreated {
    let mut tx = world.tx(person).await;
    let restored = store::restore(
        &mut tx,
        &world.user(person),
        saved,
        from_version,
        base_version,
        unlimited,
    )
    .await;
    finish(tx, restored).await.expect("restore")
}

/// One test at a time: testkit shares one Postgres among the tests of a process while any of them
/// holds it, and each test here creates the product database and the Job's roles in it from
/// nothing. Held for the whole test, so the cluster is gone (and the next test starts its own)
/// before the next one takes the lock: `_one` is the first binding, dropped last.
static ONE_CLUSTER: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// A throwaway Postgres with an empty product database, before any migration; `None` without a
/// container engine (testkit skips locally, fails in CI).
async fn empty_product() -> Option<Arc<ProductPostgres>> {
    let postgres = tls_postgres(PRODUCT_INIT).await?;
    let mut admin = PgConnection::connect_with(&postgres.superuser("postgres"))
        .await
        .expect("connect as the superuser");
    sqlx::raw_sql(CREATE_DATABASE)
        .execute(&mut admin)
        .await
        .expect("create the product database");
    admin.close().await.expect("close");
    let dir = std::env::temp_dir().join(format!(
        "assistant-dashboards-migration-{}-{}",
        std::process::id(),
        postgres.address.port()
    ));
    std::fs::create_dir_all(&dir).expect("a directory for the CA file");
    let ca_file = dir.join("postgres-ca.crt");
    std::fs::write(&ca_file, &postgres.ca_pem).expect("write the CA file");
    Some(Arc::new(ProductPostgres { postgres, ca_file }))
}

/// The store's world on `product`, through the service role (after a migration let it log in).
fn world_on(product: &Arc<ProductPostgres>) -> World {
    let db = TenantDb::connect_lazy(
        product.postgres.options("assistant_app", DATABASE),
        PgPoolOptions::new().max_connections(4),
    )
    .expect("the service role's pool verifies the server");
    World {
        product: Arc::clone(product),
        db,
        issuer: Arc::new(TestIssuer::new()),
    }
}

#[tokio::test]
async fn pages_become_optional_on_a_filled_database_and_no_row_changes() {
    let _one = ONE_CLUSTER.lock().await;
    let Some(product) = empty_product().await else {
        return;
    };

    // The previous release's schema.
    migrate(&product, &previous_release(), "up to 0003").await;
    let world = world_on(&product);
    let mut admin = world.superuser().await;
    let applied = json(&mut admin, APPLIED).await;
    assert_eq!(
        applied,
        serde_json::json!([
            "0001_assistant.sql",
            "0002_conversation_threads.sql",
            "0003_dashboards.sql",
            "0005_conversation_sharing.sql",
            "0006_answer_removed_content.sql",
            "0007_answer_drafts.sql",
            "0008_answer_drafts_index.sql",
            "0009_answer_partials.sql",
            "0010_dashboard_publications.sql"
        ])
    );
    let checks_before = json(&mut admin, CHECKS).await;
    admin.close().await.expect("close");

    // What that release wrote: drafts of two users in two tenants, published, forked,
    // published again and restored (a restore's row holds no content of its own).
    let tenant = TenantId::generate();
    let alice = world.person(tenant, TenantRole::Editor);
    let bob = world.person(tenant, TenantRole::Admin);
    let conversation = world.conversation(&alice).await;
    let draft = world
        .create_draft(&alice, conversation, &Content::new("One"))
        .await
        .dashboard_id;
    world
        .write_draft(&alice, conversation, draft, 1, None, &Content::new("Two"))
        .await
        .expect("version 2");
    let saved = publish(&world, &alice, draft, 2).await.dashboard_id;
    let bob_conversation = world.conversation(&bob).await;
    let fork = world
        .fork(&bob, bob_conversation, saved, 1, &Content::new("Bob's"))
        .await
        .expect("Bob's draft")
        .dashboard_id;
    assert_eq!(publish(&world, &bob, fork, 1).await.version, 2);
    assert_eq!(restore(&world, &bob, saved, 1, 2).await.version, 3);
    let other = world.person(TenantId::generate(), TenantRole::Editor);
    let other_conversation = world.conversation(&other).await;
    let other_draft = world
        .create_draft(&other, other_conversation, &Content::new("Elsewhere"))
        .await
        .dashboard_id;

    // 0003 refuses a version without a page (and without a runtime).
    assert_eq!(
        insert_next(&world, tenant, draft, 2, None, None, None).await,
        Some(("23514".to_owned(), "dashboard_versions_check2".to_owned()))
    );
    let mut opened_by = vec![(&alice, vec![(draft, 2), (saved, 3)])];
    opened_by.push((&bob, vec![(fork, 1), (saved, 3)]));
    opened_by.push((&other, vec![(other_draft, 1)]));
    let mut before = Vec::new();
    for (reader, dashboards) in &opened_by {
        before.push(snapshot(&world, reader, dashboards).await);
    }
    assert!(
        before[0]
            .versions
            .as_array()
            .is_some_and(|rows| rows.len() == 7),
        "{:?}",
        before[0].versions
    );

    // The next release's Job.
    migrate(&product, &PRODUCT, "every file").await;
    let mut admin = world.superuser().await;
    let applied = json(&mut admin, APPLIED).await;
    // The file under test, now among the others.
    assert_eq!(
        applied
            .as_array()
            .and_then(|names| names.get(PREVIOUS_FILES)),
        Some(&Value::from(UNDER_TEST))
    );
    let checks_after = json(&mut admin, CHECKS).await;
    admin.close().await.expect("close");

    // No row changed, and every version opens as it did.
    for ((reader, dashboards), before) in opened_by.iter().zip(&before) {
        assert_eq!(&snapshot(&world, reader, dashboards).await, before);
    }

    // Two checks replaced, nothing else.
    let mut expected = checks_before.as_object().expect("checks").clone();
    for (old, needle) in [
        (
            "dashboard_versions_check2",
            "num_nulls(runtime, title, spec, page) = 0",
        ),
        (
            "dashboard_versions_check7",
            "(octet_length(spec) + octet_length(page)) <= 57344",
        ),
    ] {
        let removed = expected.remove(old).unwrap_or_else(|| panic!("{old}"));
        assert!(
            removed
                .as_str()
                .is_some_and(|definition| definition.contains(needle)),
            "{old}: {removed}"
        );
    }
    let mut after = checks_after.as_object().expect("checks").clone();
    let content = after
        .remove("dashboard_versions_content")
        .expect("the content check");
    let size = after
        .remove("dashboard_versions_content_size")
        .expect("the size check");
    assert_eq!(after, expected);
    eprintln!("0004's checks: {content} / {size}");

    // The store writes a version of components only, which opens without page and runtime.
    let plain = Content::components("Plain");
    let written = world
        .write_draft(&alice, conversation, draft, 2, None, &plain)
        .await
        .expect("version 3 of components only");
    assert_eq!(written.version, 3);
    let mut tx = world.tx(&alice).await;
    let opened = store::open(&mut tx, alice.principal(), draft, Some(3))
        .await
        .expect("open version 3");
    tx.rollback().await.expect("roll back");
    assert_eq!(opened.version.page, None);
    assert_eq!(opened.version.runtime, None);
    assert_eq!(opened.version.spec, plain.spec);

    // Page and runtime: together or neither. A spec over the limit without a page is refused.
    let page = Some("<main>page</main>");
    for (runtime, spec, page, refused) in [
        (Some(1), None, None, true),
        (None, None, page, true),
        (None, None, None, false),
        (Some(1), None, page, false),
    ] {
        let expected =
            refused.then(|| ("23514".to_owned(), "dashboard_versions_content".to_owned()));
        assert_eq!(
            insert_next(&world, tenant, draft, 3, runtime, spec, page).await,
            expected,
            "runtime {runtime:?}, page {page:?}"
        );
    }
    let at_limit = spec_of(MAX_CONTENT_BYTES);
    let over_limit = spec_of(MAX_CONTENT_BYTES + 1);
    assert_eq!(
        insert_next(&world, tenant, draft, 3, None, Some(&at_limit), None).await,
        None
    );
    assert_eq!(
        insert_next(&world, tenant, draft, 3, None, Some(&over_limit), None).await,
        Some((
            "23514".to_owned(),
            "dashboard_versions_content_size".to_owned()
        ))
    );
    let with_page = spec_of(MAX_CONTENT_BYTES - "<main>page</main>".len() + 1);
    assert_eq!(
        insert_next(&world, tenant, draft, 3, Some(1), Some(&with_page), page).await,
        Some((
            "23514".to_owned(),
            "dashboard_versions_content_size".to_owned()
        ))
    );

    // Run again: nothing left to apply, and the rows stay.
    let mut admin = world.superuser().await;
    let versions = json(&mut admin, VERSIONS).await;
    admin.close().await.expect("close");
    migrate(&product, &PRODUCT, "again").await;
    let mut admin = world.superuser().await;
    assert_eq!(json(&mut admin, VERSIONS).await, versions);
    assert_eq!(json(&mut admin, APPLIED).await, applied);
    admin.close().await.expect("close");
}

/// The file under test of `publications_are_recorded_from_now_on_and_no_row_changes`.
const PUBLICATIONS: &str = "0010_dashboard_publications.sql";

/// The registry of the release before [`PUBLICATIONS`]: the product with every assistant file but
/// that one, which is the last, in order.
fn release_before_publications() -> Registry {
    static MIGRATIONS: OnceLock<Vec<Migration>> = OnceLock::new();
    static SERVICES: OnceLock<Vec<Service>> = OnceLock::new();
    let services = SERVICES.get_or_init(|| {
        PRODUCT
            .services
            .iter()
            .map(|service| {
                let mut service = *service;
                if service.name == "assistant" {
                    let all = service.migrations;
                    assert_eq!(
                        all.last().map(|migration| migration.name),
                        Some(PUBLICATIONS),
                        "{PUBLICATIONS} is the assistant's last file"
                    );
                    service.migrations = MIGRATIONS.get_or_init(|| all[..all.len() - 1].to_vec());
                }
                service
            })
            .collect()
    });
    Registry {
        services: services.as_slice(),
        ..PRODUCT
    }
}

/// `$1` tenant, `$2` saved dashboard, `$3` its first publisher: a new saved dashboard's row, as
/// the previous release's first publish wrote it.
const SAVED_ROW_BEFORE: &str = "INSERT INTO assistant.dashboards
    (tenant_id, dashboard_id, kind, created_by)
VALUES ($1::uuid, $2::uuid, 'saved', $3::uuid)";

/// `$1` tenant, `$2` draft, `$3` its version, `$4` saved dashboard, `$5` the user: the previous
/// release's publish of the draft's version, a copy of its content as the saved dashboard's next
/// version, which is returned. No record of where it came from.
const PUBLISH_BEFORE: &str = "INSERT INTO assistant.dashboard_versions
    (tenant_id, dashboard_id, version, origin, runtime, title, description, spec, page,
     created_by)
SELECT c.tenant_id, $4::uuid,
    (SELECT coalesce(max(version), 0) + 1 FROM assistant.dashboard_versions
     WHERE tenant_id = $1::uuid AND dashboard_id = $4::uuid),
    'publish', c.runtime, c.title, c.description, c.spec, c.page, $5::uuid
FROM assistant.dashboard_versions v
JOIN assistant.dashboard_versions c
    ON c.tenant_id = v.tenant_id AND c.dashboard_id = v.dashboard_id AND c.version = v.content_at
WHERE v.tenant_id = $1::uuid AND v.dashboard_id = $2::uuid AND v.version = $3
RETURNING version";

/// `$1` tenant, `$2` draft, `$3` saved dashboard, `$4` its version: the draft's link, as the
/// previous release's publish of the draft's latest version left it.
const LINK_BEFORE: &str = "UPDATE assistant.dashboards
SET saved_dashboard_id = $3::uuid, base_saved_version = $4
WHERE tenant_id = $1::uuid AND dashboard_id = $2::uuid";

/// What the previous release's publish of the draft's latest `version` wrote, in one transaction
/// as the superuser: the saved dashboard's row the first time (`saved` is `None`), the copy, and
/// the draft's link. Returns the saved dashboard and its new version.
async fn publish_before(
    world: &World,
    person: &Person,
    draft: DashboardId,
    version: u32,
    saved: Option<DashboardId>,
) -> (DashboardId, u32) {
    let tenant = person.tenant().to_string();
    let user = person.claims.sub.to_string();
    let mut admin = world.superuser().await;
    let mut tx = admin.begin().await.expect("begin");
    let saved = match saved {
        Some(saved) => saved,
        None => {
            let saved = DashboardId::generate();
            sqlx::query(SAVED_ROW_BEFORE)
                .bind(&tenant)
                .bind(saved.to_string())
                .bind(&user)
                .execute(&mut *tx)
                .await
                .expect("the saved dashboard's row");
            saved
        }
    };
    let written: i32 = sqlx::query_scalar(PUBLISH_BEFORE)
        .bind(&tenant)
        .bind(draft.to_string())
        .bind(i32::try_from(version).expect("small"))
        .bind(saved.to_string())
        .bind(&user)
        .fetch_one(&mut *tx)
        .await
        .expect("the copy");
    sqlx::query(LINK_BEFORE)
        .bind(&tenant)
        .bind(draft.to_string())
        .bind(saved.to_string())
        .bind(written)
        .execute(&mut *tx)
        .await
        .expect("the draft's link");
    tx.commit().await.expect("commit");
    admin.close().await.expect("close");
    (saved, u32::try_from(written).expect("a version"))
}

/// Every record of a publish's source, as the superuser reads them.
const RECORDS: &str = "SELECT coalesce(jsonb_agg(to_jsonb(p)
    ORDER BY tenant_id, draft_id, draft_version), '[]')::text
    FROM assistant.dashboard_publications p";

/// The SQLSTATE of `statement` with `binds` as the service role in `person`'s tenant, rolled
/// back; `None` when it ran.
async fn as_service_role(
    world: &World,
    person: &Person,
    statement: &'static str,
    binds: &[String],
) -> Option<String> {
    let mut tx = world.tx(person).await;
    let mut query = sqlx::query(statement);
    for bind in binds {
        query = query.bind(bind);
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

/// Acceptance criterion 4 of Laysi/trustiot-v2#1096: 0010 on a database the previous release
/// filled (see the module's documentation).
#[tokio::test]
async fn publications_are_recorded_from_now_on_and_no_row_changes() {
    let _one = ONE_CLUSTER.lock().await;
    let Some(product) = empty_product().await else {
        return;
    };

    // The previous release's schema: every file but 0010, which is the last.
    let previous = release_before_publications();
    migrate(&product, &previous, "up to 0009").await;
    let world = world_on(&product);
    let mut admin = world.superuser().await;
    let applied = json(&mut admin, APPLIED).await;
    assert!(
        applied
            .as_array()
            .is_some_and(|names| names.len() == 9 && !names.contains(&Value::from(PUBLICATIONS))),
        "{applied}"
    );
    admin.close().await.expect("close");

    // What that release wrote: Alice's draft published, Bob's draft of it published over it, a
    // restore of the saved dashboard, and another tenant's draft.
    let tenant = TenantId::generate();
    let alice = world.person(tenant, TenantRole::Editor);
    let bob = world.person(tenant, TenantRole::Admin);
    let conversation = world.conversation(&alice).await;
    let draft = world
        .create_draft(&alice, conversation, &Content::new("One"))
        .await
        .dashboard_id;
    world
        .write_draft(&alice, conversation, draft, 1, None, &Content::new("Two"))
        .await
        .expect("version 2");
    let (saved, first) = publish_before(&world, &alice, draft, 2, None).await;
    assert_eq!(first, 1);
    let bob_conversation = world.conversation(&bob).await;
    let fork = world
        .fork(&bob, bob_conversation, saved, 1, &Content::new("Bob's"))
        .await
        .expect("Bob's draft")
        .dashboard_id;
    assert_eq!(
        publish_before(&world, &bob, fork, 1, Some(saved)).await,
        (saved, 2)
    );
    assert_eq!(restore(&world, &bob, saved, 1, 2).await.version, 3);
    let other = world.person(TenantId::generate(), TenantRole::Editor);
    let other_conversation = world.conversation(&other).await;
    let other_draft = world
        .create_draft(&other, other_conversation, &Content::new("Elsewhere"))
        .await
        .dashboard_id;
    // Before 0010 the store opens a saved dashboard as it did; opening a draft's version reads
    // 0010's table, which the previous release's store did not.
    let saved_before = snapshot(&world, &alice, &[(saved, 3)]).await;
    assert!(
        saved_before
            .versions
            .as_array()
            .is_some_and(|rows| rows.len() == 7),
        "{:?}",
        saved_before.versions
    );

    // The next release's Job.
    migrate(&product, &PRODUCT, "every file").await;
    let mut admin = world.superuser().await;
    let applied = json(&mut admin, APPLIED).await;
    assert_eq!(
        applied.as_array().and_then(|names| names.last()),
        Some(&Value::from(PUBLICATIONS))
    );
    assert_eq!(json(&mut admin, RECORDS).await, serde_json::json!([]));
    admin.close().await.expect("close");

    // No row changed, the saved dashboard opens as it did, and no draft version says what it was
    // published as: the source of a publish before 0010 is unknown.
    assert_eq!(snapshot(&world, &alice, &[(saved, 3)]).await, saved_before);
    for (reader, dashboard, latest) in [
        (&alice, draft, 2),
        (&bob, fork, 1),
        (&other, other_draft, 1),
    ] {
        for version in 1..=latest {
            let mut tx = world.tx(reader).await;
            let opened = store::open(&mut tx, reader.principal(), dashboard, Some(version))
                .await
                .unwrap_or_else(|error| panic!("open {dashboard} {version}: {error}"));
            tx.rollback().await.expect("roll back");
            assert_eq!(opened.published_as, None, "{dashboard} {version}");
        }
    }

    // A publish from now on records its source: Alice's next version, merged with the saved
    // dashboard's version 3, becomes its version 4, and opening it says so.
    world
        .write_draft(
            &alice,
            conversation,
            draft,
            2,
            Some(3),
            &Content::new("Three"),
        )
        .await
        .expect("version 3");
    let published = publish(&world, &alice, draft, 3).await;
    assert_eq!((published.dashboard_id, published.version), (saved, 4));
    let mut tx = world.tx(&alice).await;
    let opened = store::open(&mut tx, alice.principal(), draft, Some(3))
        .await
        .expect("open version 3");
    tx.rollback().await.expect("roll back");
    assert_eq!(
        opened.published_as,
        Some(PublishedAs {
            dashboard_id: saved,
            version: 4
        })
    );
    let mut admin = world.superuser().await;
    let records = json(&mut admin, RECORDS).await;
    admin.close().await.expect("close");
    assert_eq!(
        records,
        serde_json::json!([{
            "tenant_id": tenant,
            "draft_id": draft,
            "draft_version": 3,
            "saved_dashboard_id": saved,
            "saved_version": 4,
            "draft_kind": "draft",
            "saved_kind": "saved"
        }])
    );

    // The service role neither changes nor deletes a record, and no role changes one.
    let keys = [tenant.to_string(), draft.to_string()];
    for statement in [
        "UPDATE assistant.dashboard_publications SET saved_version = saved_version
            WHERE tenant_id = $1::uuid AND draft_id = $2::uuid",
        "DELETE FROM assistant.dashboard_publications
            WHERE tenant_id = $1::uuid AND draft_id = $2::uuid",
    ] {
        assert_eq!(
            as_service_role(&world, &alice, statement, &keys).await,
            Some("42501".to_owned()),
            "{statement}"
        );
    }
    let mut admin = world.superuser().await;
    let changed = sqlx::query(
        "UPDATE assistant.dashboard_publications SET saved_version = saved_version
        WHERE draft_id = $1::uuid",
    )
    .bind(draft.to_string())
    .execute(&mut admin)
    .await
    .expect_err("the guard refuses every role");
    assert_eq!(
        changed
            .as_database_error()
            .and_then(|database| database.code())
            .as_deref(),
        Some("42501")
    );
    assert_eq!(json(&mut admin, RECORDS).await, records);
    admin.close().await.expect("close");
}
