//! The assets service is ready only while its database has every migration file this release
//! expects (Laysi/trustiot-v2#402): the real binary on the product's schema, applied by the
//! migration Job itself (`testkit::product`), with the schema's copy of the records changed by the
//! superuser to stand for a database that lacks a file or applied another version of it. Also:
//! each service role reads its own schema's records only.
//!
//! Starts a throwaway Postgres with TLS (podman or docker); skipped without either unless `CI`
//! is set. One test: the binary has its own product database and changes its records.

mod support;

use assets::schema::SCHEMA;
use serde_json::Value;
use sqlx::{Connection, PgConnection};
use support::{START_LIMIT, Service};
use testkit::identity::TestIssuer;
use testkit::product::{DATABASE, product_postgres};

/// The message the check logs at ERROR while files are missing or changed.
const BEHIND: &str = "the database lacks migrations this image expects, so the service stays \
                      unready; run the migration Job of this release (tools/migrate, \
                      docs/runbooks/migrations.md)";
/// The message it logs once every file is there.
const COMPLETE: &str =
    "the database has every migration this image expects (newer ones are forward compatible)";

const DELETE_RECORD: &str =
    "DELETE FROM assets.trustiot_migrate_applied WHERE component = $1 AND name = $2";
const CHANGE_RECORD: &str = "UPDATE assets.trustiot_migrate_applied SET sha256 = $3
    WHERE component = $1 AND name = $2";
/// Puts a record back as the Job wrote it, from its own table.
const RESTORE_RECORD: &str = "INSERT INTO assets.trustiot_migrate_applied (component, name, sha256)
    SELECT component, name, sha256 FROM trustiot_migrate.applied WHERE component = $1 AND name = $2
    ON CONFLICT (component, name) DO UPDATE SET sha256 = EXCLUDED.sha256";
/// A file of a later release, as the Job of that release would record it after a rollback.
const NEWER_RECORD: &str = "INSERT INTO assets.trustiot_migrate_applied (component, name, sha256)
    VALUES ('assets', '9999_later_release.sql', repeat('a', 64))";
const DROP_NEWER: &str =
    "DELETE FROM assets.trustiot_migrate_applied WHERE name = '9999_later_release.sql'";
const COPY: &str = "SELECT component, name, sha256 FROM assets.trustiot_migrate_applied
    ORDER BY component, name";
const JOB_RECORDS: &str = "SELECT component, name, sha256 FROM trustiot_migrate.applied
    WHERE component IN ('assets', 'assets.outbox') ORDER BY component, name";

const READ_OWN: &str = "SELECT count(*) FROM assets.trustiot_migrate_applied";
const READ_IDENTITY: &str = "SELECT count(*) FROM identity.trustiot_migrate_applied";
const READ_ASSETS: &str = "SELECT count(*) FROM assets.trustiot_migrate_applied";
const READ_JOB: &str = "SELECT count(*) FROM trustiot_migrate.applied";
const INSERT_OWN: &str = "INSERT INTO assets.trustiot_migrate_applied (component, name, sha256)
    VALUES ('assets', '9999_forged.sql', repeat('0', 64))";
const UPDATE_OWN: &str = "UPDATE assets.trustiot_migrate_applied SET sha256 = repeat('0', 64)";
const DELETE_OWN: &str = "DELETE FROM assets.trustiot_migrate_applied";
/// Whether a role reaches identity's schema at all (the consumer's platform role does, for the
/// outbox events), so the refusal below is the table's, not the schema's.
const USES_IDENTITY_SCHEMA: &str = "SELECT has_schema_privilege('identity', 'USAGE')";

/// SQLSTATE `insufficient_privilege`.
const INSUFFICIENT_PRIVILEGE: &str = "42501";

async fn run(admin: &mut PgConnection, statement: &'static str, args: &[&str]) {
    let mut query = sqlx::query(statement);
    for arg in args {
        query = query.bind(*arg);
    }
    query
        .execute(admin)
        .await
        .unwrap_or_else(|error| panic!("{statement}: {error}"));
}

/// Probes `/readyz` (the check runs only when readiness is asked for, as the kubelet does)
/// until the check's ERROR line appears, and returns its fields.
async fn behind(service: &Service) -> Value {
    support::wait_for("the readiness check's ERROR line", START_LIMIT, || async {
        let _ = service.readiness().await;
        service.stdout.messages(BEHIND).into_iter().next()
    })
    .await["fields"]
        .clone()
}

fn sqlstate(error: &sqlx::Error) -> Option<String> {
    error
        .as_database_error()
        .and_then(|error| error.code())
        .map(|code| code.into_owned())
}

async fn login(product: &testkit::product::ProductPostgres, role: &str) -> PgConnection {
    PgConnection::connect_with(&product.postgres.options(role, DATABASE))
        .await
        .unwrap_or_else(|error| panic!("log in as {role}: {error}"))
}

/// Runs `statement` as `conn` and expects `insufficient_privilege`.
async fn refused(conn: &mut PgConnection, role: &str, statement: &'static str) {
    let error = sqlx::query(statement)
        .execute(&mut *conn)
        .await
        .expect_err(statement);
    assert_eq!(
        sqlstate(&error).as_deref(),
        Some(INSUFFICIENT_PRIVILEGE),
        "{role}: {statement}: {error}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn readiness_follows_the_migration_records() {
    let Some(product) = product_postgres(&["identity_app", "assets_app", "assets_platform"]).await
    else {
        return;
    };
    let issuer = TestIssuer::new();
    let mut admin = product.superuser().await;

    // 0. The Job wrote the schema's copy exactly as its own records, and that is the list the
    //    image expects.
    let copy: Vec<(String, String, String)> = sqlx::query_as(COPY)
        .fetch_all(&mut admin)
        .await
        .expect("read the copy");
    let job: Vec<(String, String, String)> = sqlx::query_as(JOB_RECORDS)
        .fetch_all(&mut admin)
        .await
        .expect("read the Job's records");
    assert_eq!(copy, job);
    // The image lists the files in the order they apply (the outbox's before the service's own,
    // since assets produces events): compare both lists sorted the same way, by component, name
    // and hash, without relying on the database's collation.
    let mut expected: Vec<(String, String, String)> = SCHEMA
        .expected()
        .into_iter()
        .map(|record| (record.component, record.name, record.sha256))
        .collect();
    expected.sort();
    let mut applied = copy;
    applied.sort();
    assert_eq!(applied, expected, "the Job applied what the image expects");

    // 1. Every file is there: ready, and the check says so.
    let ready = Service::start(&product, &issuer.jwks()).await;
    support::wait_for("the check's success line", START_LIMIT, || async {
        ready.stdout.messages(COMPLETE).into_iter().next()
    })
    .await;

    // 2. A database without 0001_assets.sql: a new process stays unready and names the file. The
    //    process that already passed keeps its result.
    run(&mut admin, DELETE_RECORD, &["assets", "0001_assets.sql"]).await;
    let lacking = Service::spawn(&product, &issuer.jwks()).await;
    let fields = behind(&lacking).await;
    assert_eq!(fields["missing"], "assets/0001_assets.sql", "{fields}");
    assert_eq!(fields["changed"], "none", "{fields}");
    assert_eq!(fields["no_records"], false, "{fields}");
    lacking.wait_readiness(503).await;
    ready.wait_ready().await;

    // 3. The Job applies it (here: its record comes back): the same process turns ready.
    run(&mut admin, RESTORE_RECORD, &["assets", "0001_assets.sql"]).await;
    lacking.wait_ready().await;
    drop(lacking);

    // 4. The file applied in another version: unready, named as changed.
    let other = "0".repeat(64);
    run(
        &mut admin,
        CHANGE_RECORD,
        &["assets", "0001_assets.sql", &other],
    )
    .await;
    let changed = Service::spawn(&product, &issuer.jwks()).await;
    let fields = behind(&changed).await;
    assert_eq!(fields["missing"], "none", "{fields}");
    let text = fields["changed"].as_str().unwrap_or_default();
    assert!(
        text.starts_with("assets/0001_assets.sql (image ")
            && text.ends_with(&format!("database {other})")),
        "{fields}"
    );
    changed.wait_readiness(503).await;
    run(&mut admin, RESTORE_RECORD, &["assets", "0001_assets.sql"]).await;
    changed.wait_ready().await;
    drop(changed);

    // 5. A database newer than the image (the image was rolled back): ready. Migrations are
    //    forward compatible only, so the older code runs on the newer schema.
    run(&mut admin, NEWER_RECORD, &[]).await;
    let older_image = Service::start(&product, &issuer.jwks()).await;
    let complete = support::wait_for("the check's success line", START_LIMIT, || async {
        older_image.stdout.messages(COMPLETE).into_iter().next()
    })
    .await;
    assert_eq!(
        complete["fields"]["newer"], "assets/9999_later_release.sql",
        "{complete}"
    );
    drop(older_image);
    run(&mut admin, DROP_NEWER, &[]).await;
    drop(ready);

    // 6. Each service role reads only its own schema's records, never the Job's table, and
    //    changes none. assets_platform reaches identity's schema (it reads identity's outbox
    //    events), so its refusal on identity's records is the table's own.
    let mut assets_app = login(&product, "assets_app").await;
    let own: i64 = sqlx::query_scalar(READ_OWN)
        .fetch_one(&mut assets_app)
        .await
        .expect("assets_app reads its records");
    assert_eq!(own, i64::try_from(expected.len()).expect("a small count"));
    for statement in [READ_IDENTITY, READ_JOB, INSERT_OWN, UPDATE_OWN, DELETE_OWN] {
        refused(&mut assets_app, "assets_app", statement).await;
    }
    assets_app.close().await.expect("close");

    let mut assets_platform = login(&product, "assets_platform").await;
    let reaches: bool = sqlx::query_scalar(USES_IDENTITY_SCHEMA)
        .fetch_one(&mut assets_platform)
        .await
        .expect("read the schema privilege");
    assert!(reaches, "assets_platform reads identity's outbox events");
    for statement in [READ_IDENTITY, READ_ASSETS, READ_JOB] {
        refused(&mut assets_platform, "assets_platform", statement).await;
    }
    assets_platform.close().await.expect("close");

    let mut identity_app = login(&product, "identity_app").await;
    for statement in [READ_ASSETS, READ_JOB] {
        refused(&mut identity_app, "identity_app", statement).await;
    }
    identity_app.close().await.expect("close");
}
