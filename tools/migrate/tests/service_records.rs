//! The migration records in each service's schema (Laysi/trustiot-v2#402), with a test
//! registry and service-kit's readiness check (`service_kit::migrations::MigrationCheck`) as the
//! service role: the Job writes a file's record in the schema in the same transaction as its own;
//! a database from before the copy gets it on the next run; a copy changed by hand is set back;
//! a service whose image expects a file the database lacks is not ready until the Job of its
//! release has run; and only the service's own role reads the copy, which it cannot change.
//!
//! Needs podman or docker; skipped without one unless `CI` is set. One test: roles are
//! cluster-wide, and this binary has its own Postgres.

mod support;

use migrate::registry::{Migration, Outbox, Registry, Service};
use service_kit::migrations::{MigrationCheck, MigrationCheckError, Schema};
use service_kit::postgres::{SessionRole, check_session, connect_lazy};
use sqlx::postgres::PgPoolOptions;
use sqlx::{Connection, PgConnection, PgPool};
use support::postgres::tls_postgres;
use support::{Database, MIGRATOR, MIGRATOR_INIT, snapshot};

const CREATE: &str = "CREATE DATABASE migrate_records OWNER trustiot_owner";

const FIRST: Migration = Migration {
    name: "0001_first.sql",
    sql: "CREATE TABLE first_items (id integer PRIMARY KEY);",
};
const SECOND: Migration = Migration {
    name: "0002_second.sql",
    sql: "CREATE TABLE second_items (id integer PRIMARY KEY);",
};
/// A file that fails half-way: nothing of it, and no record, may stay.
const BROKEN: Migration = Migration {
    name: "0003_broken.sql",
    sql: "CREATE TABLE third_items (id integer PRIMARY KEY); SELECT 1 / 0;",
};

/// A service whose name has a `-`, so its schema (`record_keeper`) and its component
/// (`record-keeper`) differ; an outbox producer, so its schema also gets the outbox templates.
const fn keeper(migrations: &'static [Migration]) -> Service {
    Service {
        name: "record-keeper",
        platform_role: true,
        outbox: Outbox {
            producer: true,
            consumes: &[],
        },
        migrations,
    }
}

/// It reads the keeper's events as its platform role, so that role reaches the keeper's schema.
const NEIGHBOUR: Service = Service {
    name: "neighbour",
    platform_role: true,
    outbox: Outbox {
        producer: false,
        consumes: &["record-keeper"],
    },
    migrations: &[],
};

/// The release before: the keeper has one file.
const OLD: Registry = Registry {
    services: &[keeper(&[FIRST]), NEIGHBOUR],
    clickhouse: &[],
    tenant_role_hooks: &[],
};
/// The release after: a second file.
const NEW: Registry = Registry {
    services: &[keeper(&[FIRST, SECOND]), NEIGHBOUR],
    clickhouse: &[],
    tenant_role_hooks: &[],
};
/// A release whose third file fails.
const BROKEN_RELEASE: Registry = Registry {
    services: &[keeper(&[FIRST, SECOND, BROKEN]), NEIGHBOUR],
    clickhouse: &[],
    tenant_role_hooks: &[],
};

const LOGINS: [&str; 3] = ["record_keeper_app", "neighbour_app", "neighbour_platform"];

const COPY: &str = "SELECT component, name, sha256 FROM record_keeper.trustiot_migrate_applied
    ORDER BY component, name";
const JOB_RECORDS: &str = "SELECT component, name, sha256 FROM trustiot_migrate.applied
    WHERE component IN ('record-keeper', 'record-keeper.outbox') ORDER BY component, name";
const BROKEN_RECORDS: &str = "SELECT
    (SELECT count(*) FROM trustiot_migrate.applied WHERE name = '0003_broken.sql')
    + (SELECT count(*) FROM record_keeper.trustiot_migrate_applied WHERE name = '0003_broken.sql')";
/// The copy's owner and privileges.
const COPY_ACL: &str = "SELECT pg_get_userbyid(relowner)::text, relacl::text[]
    FROM pg_class WHERE oid = 'record_keeper.trustiot_migrate_applied'::regclass";
/// A database whose Job ran before the copy existed.
const BEFORE_THE_COPY: &str = "DROP TABLE record_keeper.trustiot_migrate_applied";
/// The copy changed by hand: a record the Job never wrote and a hash that differs from the Job's.
const TAMPER: &str = "
INSERT INTO record_keeper.trustiot_migrate_applied (component, name, sha256)
VALUES ('record-keeper', '9999_forged.sql', repeat('0', 64));
UPDATE record_keeper.trustiot_migrate_applied SET sha256 = repeat('1', 64)
WHERE name = '0001_first.sql';
";

const READ_OWN: &str = "SELECT count(*) FROM trustiot_migrate_applied";
const READ_KEEPER: &str = "SELECT count(*) FROM record_keeper.trustiot_migrate_applied";
const READ_JOB: &str = "SELECT count(*) FROM trustiot_migrate.applied";
const INSERT_OWN: &str = "INSERT INTO trustiot_migrate_applied (component, name, sha256)
    VALUES ('record-keeper', '9999_forged.sql', repeat('0', 64))";
const UPDATE_OWN: &str = "UPDATE trustiot_migrate_applied SET sha256 = repeat('0', 64)";
const DELETE_OWN: &str = "DELETE FROM trustiot_migrate_applied";
const USES_KEEPER_SCHEMA: &str = "SELECT has_schema_privilege('record_keeper', 'USAGE')";

/// SQLSTATE `insufficient_privilege`.
const INSUFFICIENT_PRIVILEGE: &str = "42501";

type Rows = Vec<(String, String, String)>;

async fn rows(admin: &mut PgConnection, query: &'static str) -> Rows {
    sqlx::query_as(query)
        .fetch_all(admin)
        .await
        .unwrap_or_else(|error| panic!("{query}: {error}"))
}

/// The keeper's service role's pool, with service-kit's session check, as the service opens it.
fn keeper_pool(db: &Database) -> PgPool {
    let options = db
        .postgres
        .options("record_keeper_app", &db.name)
        .password(&db.password("record_keeper_app"));
    connect_lazy(options, PgPoolOptions::new().max_connections(1)).expect("verify-full options")
}

/// What an image of `registry` expects of the keeper's schema.
fn image(registry: &Registry) -> Schema {
    registry
        .service("record-keeper")
        .expect("the keeper is registered")
        .expected()
}

fn missing(error: &MigrationCheckError) -> Vec<String> {
    match error {
        MigrationCheckError::Behind(behind) => behind
            .comparison
            .missing
            .iter()
            .map(ToString::to_string)
            .collect(),
        other => panic!("not a missing migration: {other}"),
    }
}

fn sqlstate(error: &sqlx::Error) -> Option<String> {
    error
        .as_database_error()
        .and_then(|error| error.code())
        .map(|code| code.into_owned())
}

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
async fn each_schema_keeps_its_records_and_the_service_waits_for_them() {
    let Some(postgres) = tls_postgres(MIGRATOR_INIT).await else {
        return;
    };
    let db = Database::create(postgres, CREATE, "migrate_records").await;
    for role in LOGINS {
        db.bind(role);
    }
    let mut admin = PgConnection::connect_with(&db.postgres.superuser(&db.name))
        .await
        .expect("connect as the superuser");

    // 1. The release before: the copy is created and holds exactly the Job's records of the
    //    keeper, its outbox templates included; the neighbour (no files) gets an empty one.
    let report = migrate::run(&db.config(MIGRATOR), &OLD)
        .await
        .expect("the run of the release before");
    for expected in [
        "created the migration records record_keeper.trustiot_migrate_applied",
        "created the migration records neighbour.trustiot_migrate_applied",
        "applied record-keeper.outbox/outbox-event.sql",
        "applied record-keeper/0001_first.sql",
    ] {
        assert!(
            report.changes.iter().any(|change| change == expected),
            "missing change {expected:?} in {:?}",
            report.changes
        );
    }
    let copy = rows(&mut admin, COPY).await;
    assert_eq!(copy, rows(&mut admin, JOB_RECORDS).await);
    assert_eq!(copy.len(), 3, "two outbox templates and one file: {copy:?}");
    let (owner, acl): (String, Vec<String>) = sqlx::query_as(COPY_ACL)
        .fetch_one(&mut admin)
        .await
        .expect("read the copy's privileges");
    assert_eq!(owner, "record_keeper_owner");
    assert_eq!(
        acl,
        [
            "record_keeper_owner=arwdDxtm/record_keeper_owner",
            "record_keeper_app=r/record_keeper_owner",
        ],
        "the owner, and SELECT for the service role only"
    );

    // 2. The image of the release after expects 0002_second.sql: not ready, and the file is
    //    named. The image of the release before is ready on the same database.
    let pool = keeper_pool(&db);
    let newer_image = MigrationCheck::new(&image(&NEW), pool.clone());
    let error = newer_image
        .check()
        .await
        .expect_err("the database lacks a file of the newer image");
    assert_eq!(missing(&error), ["record-keeper/0002_second.sql"]);
    assert!(
        error.to_string().contains("run the migration Job"),
        "{error}"
    );
    MigrationCheck::new(&image(&OLD), pool.clone())
        .check()
        .await
        .expect("the image of the release before has every file");

    // 3. The Job of the release after runs: the waiting check passes, without a restart.
    let report = migrate::run(&db.config(MIGRATOR), &NEW)
        .await
        .expect("the run of the release after");
    assert!(
        report
            .changes
            .contains(&"applied record-keeper/0002_second.sql".to_owned()),
        "{:?}",
        report.changes
    );
    newer_image
        .check()
        .await
        .expect("every file is there after the Job");
    assert_eq!(
        rows(&mut admin, COPY).await,
        rows(&mut admin, JOB_RECORDS).await
    );

    // 4. A database newer than the image (the image was rolled back) stays ready: forward
    //    compatible.
    MigrationCheck::new(&image(&OLD), pool.clone())
        .check()
        .await
        .expect("an older image on a newer database");

    // 5. A file that fails leaves neither record: both are written in its transaction.
    migrate::run(&db.config(MIGRATOR), &BROKEN_RELEASE)
        .await
        .expect_err("a file that fails stops the run");
    let broken: i64 = sqlx::query_scalar(BROKEN_RECORDS)
        .fetch_one(&mut admin)
        .await
        .expect("count the broken file's records");
    assert_eq!(broken, 0);

    // 6. A second run changes nothing.
    let before = snapshot(&db).await;
    let report = migrate::run(&db.config(MIGRATOR), &NEW)
        .await
        .expect("the second run");
    assert!(report.changes.is_empty(), "{:?}", report.changes);
    assert_eq!(snapshot(&db).await, before);

    // 7. A database whose Job ran before the copy existed: no records are readable, so the
    //    service is not ready and every file counts as missing. The next run creates the copy
    //    and fills it from the Job's records; the run after changes nothing.
    sqlx::raw_sql(BEFORE_THE_COPY)
        .execute(&mut admin)
        .await
        .expect("drop the copy");
    let error = MigrationCheck::new(&image(&NEW), pool.clone())
        .check()
        .await
        .expect_err("no records");
    match &error {
        MigrationCheckError::Behind(behind) => assert!(behind.no_records, "{error}"),
        other => panic!("{other}"),
    }
    assert_eq!(missing(&error).len(), 4, "{error}");
    let report = migrate::run(&db.config(MIGRATOR), &NEW)
        .await
        .expect("the run that adds the copy");
    assert_eq!(
        report.changes,
        [
            "created the migration records record_keeper.trustiot_migrate_applied",
            "copied 4 migration records of record-keeper into \
             record_keeper.trustiot_migrate_applied",
        ]
    );
    assert_eq!(
        rows(&mut admin, COPY).await,
        rows(&mut admin, JOB_RECORDS).await
    );
    MigrationCheck::new(&image(&NEW), pool.clone())
        .check()
        .await
        .expect("ready once the copy is filled");
    let report = migrate::run(&db.config(MIGRATOR), &NEW)
        .await
        .expect("the run after");
    assert!(report.changes.is_empty(), "{:?}", report.changes);

    // 8. A copy changed by hand is set back to the Job's records.
    sqlx::raw_sql(TAMPER)
        .execute(&mut admin)
        .await
        .expect("change the copy");
    let report = migrate::run(&db.config(MIGRATOR), &NEW)
        .await
        .expect("the run that repairs the copy");
    assert_eq!(
        report.changes,
        [
            "copied 1 migration records of record-keeper into \
             record_keeper.trustiot_migrate_applied",
            "removed records that trustiot_migrate.applied does not have from \
             record_keeper.trustiot_migrate_applied: record-keeper/9999_forged.sql",
        ]
    );
    assert_eq!(
        rows(&mut admin, COPY).await,
        rows(&mut admin, JOB_RECORDS).await
    );
    pool.close().await;

    // 9. Only the keeper's service role reads the copy, and it changes nothing there; it never
    //    reads the Job's own table. The neighbour's platform role reaches the keeper's schema
    //    (it reads the keeper's events) and is still refused the copy. Every session still
    //    passes service-kit's session check (decision 29).
    let mut keeper = db
        .connect_with_password("record_keeper_app", &db.password("record_keeper_app"))
        .await
        .expect("record_keeper_app logs in");
    let own: i64 = sqlx::query_scalar(READ_OWN)
        .fetch_one(&mut keeper)
        .await
        .expect("record_keeper_app reads its records");
    assert_eq!(own, 4);
    for statement in [READ_JOB, INSERT_OWN, UPDATE_OWN, DELETE_OWN] {
        refused(&mut keeper, "record_keeper_app", statement).await;
    }
    check_session(&mut keeper, SessionRole::Service)
        .await
        .expect("run the session check")
        .expect("service-kit accepts record_keeper_app");
    keeper.close().await.expect("close");
    for role in ["neighbour_app", "neighbour_platform"] {
        let mut conn = db
            .connect_with_password(role, &db.password(role))
            .await
            .unwrap_or_else(|error| panic!("{role} logs in: {error}"));
        if role == "neighbour_platform" {
            let reaches: bool = sqlx::query_scalar(USES_KEEPER_SCHEMA)
                .fetch_one(&mut conn)
                .await
                .expect("read the schema privilege");
            assert!(
                reaches,
                "the consumer's platform role reads the keeper's events"
            );
        }
        for statement in [READ_KEEPER, READ_JOB] {
            refused(&mut conn, role, statement).await;
        }
        let kind = if role == "neighbour_platform" {
            SessionRole::Platform
        } else {
            SessionRole::Service
        };
        check_session(&mut conn, kind)
            .await
            .expect("run the session check")
            .unwrap_or_else(|problem| panic!("service-kit refuses {role}: {problem:?}"));
        conn.close().await.expect("close");
    }
    admin.close().await.expect("close");
}
