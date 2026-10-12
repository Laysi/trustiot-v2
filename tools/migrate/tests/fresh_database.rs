//! The product's Postgres migrations on a fresh database, as CI runs them (issue acceptance:
//! "CI 在全新資料庫上跑完全部 migration"): the roles of decision 29, a second run that changes
//! nothing, password rotation, and the refusals.
//!
//! Needs podman or docker (a throwaway Postgres with TLS); skipped without one unless `CI` is
//! set. One test only: roles are cluster-wide, and this binary has its own Postgres.

mod support;

use migrate::Error;
use migrate::registry::{Migration, Outbox, PRODUCT, Registry, Service};
use service_kit::postgres::{SessionRole, check_session};
use sqlx::Connection;
use support::postgres::tls_postgres;
use support::{Database, MIGRATOR, MIGRATOR_INIT, random_hex, snapshot};

const CREATE: &str = "CREATE DATABASE migrate_fresh OWNER trustiot_owner";

/// The product's identity service with its first file changed after it was applied.
const CHANGED: Registry = Registry {
    services: &[Service {
        name: "identity",
        platform_role: false,
        outbox: Outbox {
            producer: false,
            consumes: &[],
        },
        migrations: &[Migration {
            name: "0001_identity.sql",
            sql: "SELECT 1;",
        }],
    }],
    clickhouse: &[],
    tenant_role_hooks: &[],
};

/// What a run creates first: its bookkeeping schema and the identity roles.
const OWN_OBJECTS: &str =
    "SELECT (SELECT count(*) FROM pg_namespace WHERE nspname = 'trustiot_migrate')
    + (SELECT count(*) FROM pg_roles WHERE rolname LIKE 'identity\\_%')";
const ROLE_ATTRIBUTES: &str = "SELECT rolsuper, rolbypassrls, rolcanlogin, rolcreaterole,
    rolcreatedb, rolinherit FROM pg_roles WHERE rolname = $1";
const TABLES_NOT_OWNED_BY_OWNER: &str = "SELECT count(*) FROM pg_tables WHERE schemaname = 'identity' AND tableowner <> 'identity_owner'";
const TABLES_OWNED_BY_SERVICE_ROLE: &str =
    "SELECT count(*) FROM pg_tables WHERE tableowner = 'identity_app'";
const IDENTITY_TABLES: &str = "SELECT count(*) FROM pg_tables WHERE schemaname = 'identity'";
const TEMPORARY_FOR_APP: &str =
    "SELECT has_database_privilege('identity_app', current_database(), 'TEMPORARY')";
const MIGRATOR_READS_SESSIONS: &str =
    "SELECT has_table_privilege('trustiot_owner', 'identity.sessions', 'SELECT')";
const SEARCH_PATH: &str = "SELECT current_setting('search_path')";
const READ_MEMBERSHIPS: &str = "SELECT count(*) FROM memberships";
const GIVE_BYPASSRLS: &str = "ALTER ROLE identity_app BYPASSRLS";
const TAKE_BYPASSRLS: &str = "ALTER ROLE identity_app NOBYPASSRLS";
const JOIN_OWNER: &str = "GRANT identity_owner TO identity_app";
const LEAVE_OWNER: &str = "REVOKE identity_owner FROM identity_app";
const ADD_MEMBER: &str = "GRANT identity_app TO no_createrole";
const REMOVE_MEMBER: &str = "REVOKE identity_app FROM no_createrole";

#[tokio::test(flavor = "multi_thread")]
async fn a_fresh_database_gets_every_migration_once() {
    let Some(postgres) = tls_postgres(MIGRATOR_INIT).await else {
        return;
    };
    let db = Database::create(postgres, CREATE, "migrate_fresh").await;
    let first = random_hex();

    // 0. A role a service of the deployment logs in with must be in the registry and have a
    //    password file; otherwise the run stops before it changes anything.
    for (required, what) in [
        ("identity_app", "its password file is missing"),
        (
            "identity_app,nobody_app",
            "no service in tools/migrate's registry",
        ),
    ] {
        if what.contains("registry") {
            db.set_password_file("identity_app", &first);
        }
        let mut config = db.config(MIGRATOR);
        config.postgres.required_passwords = Some(required.to_owned());
        let error = migrate::run(&config, &PRODUCT)
            .await
            .expect_err("a required role without a password must stop the run");
        assert!(
            matches!(&error, Error::Role { problem, .. } if problem.contains(what)),
            "{required}: {error}"
        );
    }
    let mut admin = sqlx::PgConnection::connect_with(&db.postgres.superuser(&db.name))
        .await
        .expect("connect as the superuser");
    let touched: i64 = sqlx::query_scalar(OWN_OBJECTS)
        .fetch_one(&mut admin)
        .await
        .expect("look for the Job's objects");
    assert_eq!(touched, 0, "the refused runs created nothing");
    let mut config = db.config(MIGRATOR);
    config.postgres.required_passwords = Some("identity_app".to_owned());

    // 1. The first run applies everything.
    let report = migrate::run(&config, &PRODUCT)
        .await
        .expect("the first run");
    for expected in [
        "applied migrate/0001_helpers.sql",
        "applied database/database.sql",
        "created role identity_owner",
        "created role identity_app",
        "created schema identity owned by identity_owner",
        "set the search_path of identity_app to identity",
        "set the password of identity_app",
        "applied identity/0001_identity.sql",
    ] {
        assert!(
            report.changes.iter().any(|change| change == expected),
            "missing change {expected:?} in {:?}",
            report.changes
        );
    }
    assert!(report.roles_without_password.is_empty());

    // 2. Decision 29 in the catalogs (pg_roles and pg_tables).
    let app: (bool, bool, bool, bool, bool, bool) = sqlx::query_as(ROLE_ATTRIBUTES)
        .bind("identity_app")
        .fetch_one(&mut admin)
        .await
        .expect("read identity_app");
    assert_eq!(
        app,
        (false, false, true, false, false, false),
        "identity_app: no superuser, no BYPASSRLS, logs in, no CREATEROLE or CREATEDB, NOINHERIT"
    );
    let owner: (bool, bool, bool, bool, bool, bool) = sqlx::query_as(ROLE_ATTRIBUTES)
        .bind("identity_owner")
        .fetch_one(&mut admin)
        .await
        .expect("read identity_owner");
    assert_eq!(
        owner,
        (false, false, false, false, false, false),
        "identity_owner never logs in"
    );
    let tables: i64 = sqlx::query_scalar(IDENTITY_TABLES)
        .fetch_one(&mut admin)
        .await
        .expect("count tables");
    assert_eq!(tables, 3, "users, memberships, sessions");
    for (query, what) in [
        (
            TABLES_NOT_OWNED_BY_OWNER,
            "tables of schema identity not owned by identity_owner",
        ),
        (
            TABLES_OWNED_BY_SERVICE_ROLE,
            "tables owned by the service role",
        ),
    ] {
        let count: i64 = sqlx::query_scalar(query)
            .fetch_one(&mut admin)
            .await
            .expect("count tables");
        assert_eq!(count, 0, "{what}");
    }
    let temporary: bool = sqlx::query_scalar(TEMPORARY_FOR_APP)
        .fetch_one(&mut admin)
        .await
        .expect("read TEMPORARY");
    assert!(!temporary, "database.sql revoked TEMPORARY");
    let migrator_reads: bool = sqlx::query_scalar(MIGRATOR_READS_SESSIONS)
        .fetch_one(&mut admin)
        .await
        .expect("read the migrator's rights");
    assert!(
        !migrator_reads,
        "the migration role does not inherit the owner role's rights"
    );

    // 3. The service role passes service-kit's session check and finds its schema first.
    let mut app_conn = db
        .connect_with_password("identity_app", &first)
        .await
        .expect("identity_app logs in with its password file");
    check_session(&mut app_conn, SessionRole::Service)
        .await
        .expect("run the session check")
        .expect("service-kit accepts the identity_app session");
    let search_path: String = sqlx::query_scalar(SEARCH_PATH)
        .fetch_one(&mut app_conn)
        .await
        .expect("read search_path");
    assert_eq!(search_path, "identity");
    let members: i64 = sqlx::query_scalar(READ_MEMBERSHIPS)
        .fetch_one(&mut app_conn)
        .await
        .expect("read memberships without a tenant");
    assert_eq!(members, 0);
    app_conn.close().await.expect("close");

    // 4. A second run changes nothing.
    let before = snapshot(&db).await;
    let report = migrate::run(&db.config(MIGRATOR), &PRODUCT)
        .await
        .expect("the second run");
    assert!(report.changes.is_empty(), "{:?}", report.changes);
    assert_eq!(
        snapshot(&db).await,
        before,
        "the second run changed the catalogs"
    );

    // 5. Rotation: a new password file sets the password, and only that.
    let second = random_hex();
    db.set_password_file("identity_app", &second);
    let report = migrate::run(&db.config(MIGRATOR), &PRODUCT)
        .await
        .expect("the rotation run");
    assert_eq!(report.changes, ["set the password of identity_app"]);
    assert!(
        db.connect_with_password("identity_app", &first)
            .await
            .is_err()
    );
    db.connect_with_password("identity_app", &second)
        .await
        .expect("the new password logs in");
    let before = snapshot(&db).await;
    let report = migrate::run(&db.config(MIGRATOR), &PRODUCT)
        .await
        .expect("the run after rotation");
    assert!(report.changes.is_empty(), "{:?}", report.changes);
    assert_eq!(snapshot(&db).await, before);

    // 6. An applied file that changed stops the run.
    let error = migrate::run(&db.config(MIGRATOR), &CHANGED)
        .await
        .expect_err("a changed file must stop the run");
    assert!(matches!(error, Error::Changed { .. }), "{error}");

    // 7. The migration role must not be a superuser and must have CREATEROLE.
    let error = migrate::run(&db.config("postgres"), &PRODUCT)
        .await
        .expect_err("a superuser must be refused");
    assert!(matches!(error, Error::MigrationRole { .. }), "{error}");
    let error = migrate::run(&db.config("no_createrole"), &PRODUCT)
        .await
        .expect_err("a role without CREATEROLE must be refused");
    assert!(
        matches!(&error, Error::MigrationRole { problem, .. } if problem.contains("CREATEROLE")),
        "{error}"
    );

    // 8. A service role that gained BYPASSRLS, joined the owner role, or gained a member other
    //    than the migration role stops the run.
    for (break_it, repair, what) in [
        (GIVE_BYPASSRLS, TAKE_BYPASSRLS, "BYPASSRLS"),
        (JOIN_OWNER, LEAVE_OWNER, "membership in the owner role"),
        (
            ADD_MEMBER,
            REMOVE_MEMBER,
            "a member other than the migration role",
        ),
    ] {
        sqlx::raw_sql(break_it)
            .execute(&mut admin)
            .await
            .expect("break the role");
        let error = migrate::run(&db.config(MIGRATOR), &PRODUCT)
            .await
            .expect_err(what);
        assert!(matches!(error, Error::Role { .. }), "{what}: {error}");
        sqlx::raw_sql(repair)
            .execute(&mut admin)
            .await
            .expect("repair the role");
    }
    let report = migrate::run(&db.config(MIGRATOR), &PRODUCT)
        .await
        .expect("the run after the repairs");
    assert!(report.changes.is_empty(), "{:?}", report.changes);
}
