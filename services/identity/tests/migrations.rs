//! The identity service is ready only while its database has every migration file this release
//! expects (Laysi/trustiot-v2#402): the real binary on the product's schema, applied by the
//! migration Job itself (`testkit::product`), with the schema's copy of the records changed by the
//! superuser to stand for a database that lacks a file or applied another version of it.
//!
//! Starts a throwaway Postgres with TLS (podman or docker); skipped without either unless `CI`
//! is set. One test: the binary has its own product database and changes its records.

mod support;

use std::time::{Duration, Instant};

use identity::schema::SCHEMA;
use serde_json::{Value, json};
use sqlx::{Connection, PgConnection};
use support::{KeyFiles, Service, TestKey, request};
use testkit::postgres::TEST_PASSWORD;
use testkit::product::{DATABASE, ProductPostgres, product_postgres};

const APP: &str = "app.saas.trustiot.test";
const KID: &str = "2026-10-a";

/// Generous bounds for a loaded host (CLAUDE.md, "Test time limits"): starting the binary and
/// its first checked session, and the readiness check's first run after a change.
const READY_LIMIT: Duration = Duration::from_secs(60);
/// How often `behind` asks for readiness while it waits for the log line.
const PROBE_EVERY: Duration = Duration::from_millis(200);

/// The message the check logs at ERROR while files are missing or changed.
const BEHIND: &str = "the database lacks migrations this image expects, so the service stays \
                      unready; run the migration Job of this release (tools/migrate, \
                      docs/runbooks/migrations.md)";
/// The message it logs once every file is there.
const COMPLETE: &str =
    "the database has every migration this image expects (newer ones are forward compatible)";

const DELETE_RECORD: &str =
    "DELETE FROM identity.trustiot_migrate_applied WHERE component = $1 AND name = $2";
const CHANGE_RECORD: &str = "UPDATE identity.trustiot_migrate_applied SET sha256 = $3
    WHERE component = $1 AND name = $2";
/// Puts a record back as the Job wrote it, from its own table.
const RESTORE_RECORD: &str =
    "INSERT INTO identity.trustiot_migrate_applied (component, name, sha256)
    SELECT component, name, sha256 FROM trustiot_migrate.applied WHERE component = $1 AND name = $2
    ON CONFLICT (component, name) DO UPDATE SET sha256 = EXCLUDED.sha256";
const COPY: &str = "SELECT component, name, sha256 FROM identity.trustiot_migrate_applied
    ORDER BY component, name";
const JOB_RECORDS: &str = "SELECT component, name, sha256 FROM trustiot_migrate.applied
    WHERE component IN ('identity', 'identity.outbox') ORDER BY component, name";

const READ_OWN: &str = "SELECT count(*) FROM trustiot_migrate_applied";
const READ_JOB: &str = "SELECT count(*) FROM trustiot_migrate.applied";
const READ_ASSETS: &str = "SELECT count(*) FROM assets.trustiot_migrate_applied";
const INSERT_OWN: &str = "INSERT INTO trustiot_migrate_applied (component, name, sha256)
    VALUES ('identity', '9999_forged.sql', repeat('0', 64))";
const UPDATE_OWN: &str = "UPDATE trustiot_migrate_applied SET sha256 = repeat('0', 64)";
const DELETE_OWN: &str = "DELETE FROM trustiot_migrate_applied";

/// SQLSTATE `insufficient_privilege`.
const INSUFFICIENT_PRIVILEGE: &str = "42501";

fn start(product: &ProductPostgres) -> Service {
    let files = KeyFiles::new("migrations", &TestKey::new(40), KID);
    let routes = json!([
        {"host": APP, "path": "/api/identity", "match": "PathPrefix", "service": "identity"},
    ]);
    let mut env = files.env();
    env.extend([
        ("TRUSTIOT_INGRESS_ROUTES".into(), routes.to_string()),
        ("TRUSTIOT__DATABASE__USERNAME".into(), "identity_app".into()),
        ("TRUSTIOT__DATABASE__PASSWORD".into(), TEST_PASSWORD.into()),
    ]);
    env.extend(
        product
            .platform_env()
            .into_iter()
            .map(|(name, value)| (name.to_owned(), value)),
    );
    let env: Vec<(&str, &str)> = env.iter().map(|(n, v)| (n.as_str(), v.as_str())).collect();
    Service::start(&env)
}

/// Probes `/readyz` (the check runs only when readiness is asked for, as the kubelet does)
/// until the check's ERROR line appears, and returns its fields.
fn behind(service: &Service) -> Value {
    let deadline = Instant::now() + READY_LIMIT;
    loop {
        let _ = request(service.health_address, "GET", "/readyz", &[]);
        if let Some(line) = service.stdout.wait_for_message(BEHIND, PROBE_EVERY) {
            assert_eq!(line["level"], "ERROR", "{line}");
            return line["fields"].clone();
        }
        assert!(
            Instant::now() < deadline,
            "no `{BEHIND}` line within {READY_LIMIT:?}\nstdout:\n{}",
            service.stdout.text()
        );
    }
}

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

fn sqlstate(error: &sqlx::Error) -> Option<String> {
    error
        .as_database_error()
        .and_then(|error| error.code())
        .map(|code| code.into_owned())
}

#[tokio::test(flavor = "multi_thread")]
async fn readiness_follows_the_migration_records() {
    let Some(product) = product_postgres(&["identity_app"]).await else {
        return;
    };
    let mut admin = product.superuser().await;

    // 0. The Job wrote the schema's copy exactly as its own records, and that is the list the
    //    image expects: the outbox templates of a producer and every file.
    let copy: Vec<(String, String, String)> = sqlx::query_as(COPY)
        .fetch_all(&mut admin)
        .await
        .expect("read the copy");
    let job: Vec<(String, String, String)> = sqlx::query_as(JOB_RECORDS)
        .fetch_all(&mut admin)
        .await
        .expect("read the Job's records");
    assert_eq!(copy, job);
    let mut expected: Vec<(String, String, String)> = SCHEMA
        .expected()
        .into_iter()
        .map(|record| (record.component, record.name, record.sha256))
        .collect();
    expected.sort();
    assert_eq!(copy, expected, "the Job applied what the image expects");

    // 1. Every file is there: ready, and the check says so once.
    let ready = start(&product);
    ready.wait_ready(READY_LIMIT);
    assert!(
        ready
            .stdout
            .wait_for_message(COMPLETE, READY_LIMIT)
            .is_some(),
        "{}",
        ready.stdout.text()
    );

    // 2. A database without 0002_tenants.sql: a new process stays unready and names the file.
    //    The process that already passed keeps its result (the Job only adds records).
    run(&mut admin, DELETE_RECORD, &["identity", "0002_tenants.sql"]).await;
    let lacking = start(&product);
    let fields = behind(&lacking);
    assert_eq!(fields["missing"], "identity/0002_tenants.sql", "{fields}");
    assert_eq!(fields["changed"], "none", "{fields}");
    assert_eq!(fields["no_records"], false, "{fields}");
    lacking.wait_readiness(503, READY_LIMIT);
    ready.wait_ready(READY_LIMIT);

    // 3. The Job applies the file (here: its record comes back): the same process turns ready.
    run(
        &mut admin,
        RESTORE_RECORD,
        &["identity", "0002_tenants.sql"],
    )
    .await;
    lacking.wait_ready(READY_LIMIT);
    drop(lacking);

    // 4. An outbox template applied in another version: unready, the file named as changed.
    let other = "0".repeat(64);
    run(
        &mut admin,
        CHANGE_RECORD,
        &["identity.outbox", "outbox-aggregate.sql", &other],
    )
    .await;
    let changed = start(&product);
    let fields = behind(&changed);
    assert_eq!(fields["missing"], "none", "{fields}");
    let text = fields["changed"].as_str().unwrap_or_default();
    assert!(
        text.starts_with("identity.outbox/outbox-aggregate.sql (image ")
            && text.ends_with(&format!("database {other})")),
        "{fields}"
    );
    changed.wait_readiness(503, READY_LIMIT);
    run(
        &mut admin,
        RESTORE_RECORD,
        &["identity.outbox", "outbox-aggregate.sql"],
    )
    .await;
    changed.wait_ready(READY_LIMIT);
    drop(changed);
    drop(ready);

    // 5. The service role reads its own records and nothing else: not the Job's table, not
    //    another service's records; and it cannot change its own.
    let mut app = PgConnection::connect_with(&product.postgres.options("identity_app", DATABASE))
        .await
        .expect("log in as identity_app");
    let own: i64 = sqlx::query_scalar(READ_OWN)
        .fetch_one(&mut app)
        .await
        .expect("identity_app reads its records");
    assert_eq!(own, i64::try_from(expected.len()).expect("a small count"));
    for statement in [READ_JOB, READ_ASSETS, INSERT_OWN, UPDATE_OWN, DELETE_OWN] {
        let error = sqlx::query(statement)
            .execute(&mut app)
            .await
            .expect_err(statement);
        assert_eq!(
            sqlstate(&error).as_deref(),
            Some(INSUFFICIENT_PRIVILEGE),
            "{statement}: {error}"
        );
    }
    app.close().await.expect("close");
}
