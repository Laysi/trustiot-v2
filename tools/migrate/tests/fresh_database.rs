//! The product's Postgres migrations on a fresh database, as CI runs them (issue acceptance:
//! "CI 在全新資料庫上跑完全部 migration"): the roles of decision 29, a second run that changes
//! nothing, the generated password and its rotation, a lost or edited password, and the
//! refusals.
//!
//! Needs podman or docker (a throwaway Postgres with TLS); skipped without one unless `CI` is
//! set. One test only: roles are cluster-wide, and this binary has its own Postgres.

mod support;

use migrate::Error;
use migrate::registry::{Migration, Outbox, PRODUCT, Registry, Service};
use service_kit::postgres::{SessionRole, check_session};
use sqlx::Connection;
use support::postgres::tls_postgres;
use support::{Database, MIGRATOR, MIGRATOR_INIT, snapshot};

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
/// The relations of schema assets the event log's platform role can do anything with
/// (Laysi/trustiot-v2#130): it must read assets' events and nothing else of that schema.
const IDENTITY_PLATFORM_IN_ASSETS: &str = "SELECT c.relname::text FROM pg_class c
    JOIN pg_namespace n ON n.oid = c.relnamespace
    WHERE n.nspname = 'assets' AND c.relkind IN ('r', 'p', 'v', 'm', 'f')
      AND has_table_privilege('identity_platform', c.oid,
          'SELECT, INSERT, UPDATE, DELETE, TRUNCATE, REFERENCES, TRIGGER')
    ORDER BY 1";
/// What it may do with assets' events: read them, and no write of any kind.
const IDENTITY_PLATFORM_ON_ASSETS_EVENTS: &str = "SELECT
    has_table_privilege('identity_platform', 'assets.outbox_event', 'SELECT'),
    has_table_privilege('identity_platform', 'assets.outbox_event',
        'INSERT, UPDATE, DELETE, TRUNCATE, REFERENCES, TRIGGER'),
    has_schema_privilege('identity_platform', 'assets', 'CREATE')";
/// The relations of schema assets the certs service's platform role can do anything with
/// (Laysi/trustiot-v2#632): it reads assets' events for the gateway bindings, nothing else.
const CERTS_PLATFORM_IN_ASSETS: &str = "SELECT c.relname::text FROM pg_class c
    JOIN pg_namespace n ON n.oid = c.relnamespace
    WHERE n.nspname = 'assets' AND c.relkind IN ('r', 'p', 'v', 'm', 'f')
      AND has_table_privilege('certs_platform', c.oid,
          'SELECT, INSERT, UPDATE, DELETE, TRUNCATE, REFERENCES, TRIGGER')
    ORDER BY 1";
/// What it may do with assets' events: read them, and no write of any kind.
const CERTS_PLATFORM_ON_ASSETS_EVENTS: &str = "SELECT
    has_table_privilege('certs_platform', 'assets.outbox_event', 'SELECT'),
    has_table_privilege('certs_platform', 'assets.outbox_event',
        'INSERT, UPDATE, DELETE, TRUNCATE, REFERENCES, TRIGGER'),
    has_schema_privilege('certs_platform', 'assets', 'CREATE')";
/// The relations of schema identity the certs service's platform role can do anything with
/// (Laysi/trustiot-v2#778): it reads identity's events for the tenant states, nothing else.
const CERTS_PLATFORM_IN_IDENTITY: &str = "SELECT c.relname::text FROM pg_class c
    JOIN pg_namespace n ON n.oid = c.relnamespace
    WHERE n.nspname = 'identity' AND c.relkind IN ('r', 'p', 'v', 'm', 'f')
      AND has_table_privilege('certs_platform', c.oid,
          'SELECT, INSERT, UPDATE, DELETE, TRUNCATE, REFERENCES, TRIGGER')
    ORDER BY 1";
/// What it may do with identity's events: read them, and no write of any kind.
const CERTS_PLATFORM_ON_IDENTITY_EVENTS: &str = "SELECT
    has_table_privilege('certs_platform', 'identity.outbox_event', 'SELECT'),
    has_table_privilege('certs_platform', 'identity.outbox_event',
        'INSERT, UPDATE, DELETE, TRUNCATE, REFERENCES, TRIGGER'),
    has_schema_privilege('certs_platform', 'identity', 'CREATE')";
/// The roles the identity outbox's read policy names.
const IDENTITY_EVENT_READERS: &str = "SELECT array_to_string(polroles::regrole[], ',')
    FROM pg_policy WHERE polrelid = 'identity.outbox_event'::regclass
        AND polname = 'consumer_read'";
/// The certs service's consumer tables and its copy of the bindings and tenant statuses
/// (Laysi/trustiot-v2#632), with whether row-level security is on.
const CERTS_CONSUMER_TABLES: &str = "SELECT c.relname::text, c.relrowsecurity FROM pg_class c
    JOIN pg_namespace n ON n.oid = c.relnamespace
    WHERE n.nspname = 'certs' AND c.relname IN ('outbox_subscription', 'outbox_inbox',
        'outbox_dead_letter', 'outbox_version', 'gateway_bindings', 'tenant_statuses')
    ORDER BY 1";
/// The roles the assets outbox's read policy names.
const ASSETS_EVENT_READERS: &str = "SELECT array_to_string(polroles::regrole[], ',')
    FROM pg_policy WHERE polrelid = 'assets.outbox_event'::regclass AND polname = 'consumer_read'";
const SEARCH_PATH: &str = "SELECT current_setting('search_path')";
const READ_MEMBERSHIPS: &str = "SELECT count(*) FROM memberships";
const GIVE_BYPASSRLS: &str = "ALTER ROLE identity_app BYPASSRLS";
const TAKE_BYPASSRLS: &str = "ALTER ROLE identity_app NOBYPASSRLS";
const JOIN_OWNER: &str = "GRANT identity_owner TO identity_app";
const LEAVE_OWNER: &str = "REVOKE identity_owner FROM identity_app";
const ADD_MEMBER: &str = "GRANT identity_app TO no_createrole";
const REMOVE_MEMBER: &str = "REVOKE identity_app FROM no_createrole";
/// Run as identity_app itself: overrides the search_path the Job set for the role.
const OWN_DATABASE_SEARCH_PATH: &str =
    "ALTER ROLE identity_app IN DATABASE migrate_fresh SET search_path = public";
const RESET_DATABASE_SETTINGS: &str =
    "ALTER ROLE identity_app IN DATABASE migrate_fresh RESET search_path";
const SET_TIMEOUT: &str = "ALTER ROLE identity_app SET statement_timeout = '1h'";
const RESET_TIMEOUT: &str = "ALTER ROLE identity_app RESET statement_timeout";
const GRANT_CREATE: &str = "GRANT CREATE ON SCHEMA identity TO identity_app";
const REVOKE_CREATE: &str = "REVOKE CREATE ON SCHEMA identity FROM identity_app";
const GRANT_TEMPORARY: &str = "GRANT TEMPORARY ON DATABASE migrate_fresh TO PUBLIC";
const REVOKE_TEMPORARY: &str = "REVOKE TEMPORARY ON DATABASE migrate_fresh FROM PUBLIC";

#[tokio::test(flavor = "multi_thread")]
async fn a_fresh_database_gets_every_migration_once() {
    let Some(postgres) = tls_postgres(MIGRATOR_INIT).await else {
        return;
    };
    let db = Database::create(postgres, CREATE, "migrate_fresh").await;
    db.bind("identity_app");

    // 0. A role bound to a Secret must be a login role of the registry; otherwise the run stops
    //    before it changes anything, the Secrets included.
    let mut config = db.config(MIGRATOR);
    if let Some(passwords) = config.passwords.as_mut() {
        passwords.postgres =
            Some("identity_app=identity-app-postgres,nobody_app=nobody".to_owned());
    }
    let error = migrate::run(&config, &PRODUCT)
        .await
        .expect_err("a bound role that no service has must stop the run");
    assert!(
        matches!(&error, Error::Role { problem, .. } if problem.contains("no service in tools/migrate's registry")),
        "{error}"
    );
    assert!(
        db.secrets.seen().is_empty(),
        "the refused run touched no Secret"
    );
    let mut admin = sqlx::PgConnection::connect_with(&db.postgres.superuser(&db.name))
        .await
        .expect("connect as the superuser");
    let touched: i64 = sqlx::query_scalar(OWN_OBJECTS)
        .fetch_one(&mut admin)
        .await
        .expect("look for the Job's objects");
    assert_eq!(touched, 0, "the refused run created nothing");

    // 1. The first run applies everything; it generates the service role's password into the
    //    role's empty Secret and sets it.
    let report = migrate::run(&db.config(MIGRATOR), &PRODUCT)
        .await
        .expect("the first run");
    for expected in [
        "applied migrate/0001_helpers.sql",
        "applied database/database.sql",
        "created role identity_owner",
        "created role identity_app",
        "created schema identity owned by identity_owner",
        "set the search_path of identity_app to identity",
        "generated the password of identity_app in Secret identity-app-postgres (it had none)",
        "set the password of identity_app",
        "applied identity/0001_identity.sql",
    ] {
        assert!(
            report.changes.iter().any(|change| change == expected),
            "missing change {expected:?} in {:?}",
            report.changes
        );
    }
    // Only identity_app is bound here: this deployment gives identity's platform role no Secret
    // (the chart binds one for the event log's consumer, Laysi/trustiot-v2#130), and the assets,
    // telemetry-query, certs, assistant, ai-gateway, ingest and gateway-ops services are not part
    // of it, so
    // their roles stay NOLOGIN.
    assert_eq!(
        report.roles_without_password,
        [
            "identity_platform",
            "assets_app",
            "assets_platform",
            "telemetry_query_app",
            "telemetry_query_platform",
            "certs_app",
            "certs_platform",
            "assistant_app",
            "assistant_platform",
            "ai_gateway_app",
            "ingest_app",
            "ingest_platform",
            "gateway_ops_app",
            "gateway_ops_platform"
        ]
    );
    let first = db.password("identity_app");
    assert_eq!(first.len(), 64, "256 bits as hex digits");
    let secret = db.secrets.get("identity-app-postgres").expect("the Secret");
    assert_eq!(
        secret
            .annotations
            .get(migrate::passwords::GENERATION_ANNOTATION)
            .map(String::as_str),
        Some("1")
    );
    for (index, seen) in db.secrets.seen().iter().enumerate() {
        assert!(seen.authorized, "the run sends the ServiceAccount token");
        let expected = if index == 1 {
            "PATCH /api/v1/namespaces/release-ns/secrets/identity-app-postgres?fieldManager=trustiot-migrate HTTP/1.1"
        } else {
            "GET /api/v1/namespaces/release-ns/secrets/identity-app-postgres HTTP/1.1"
        };
        assert_eq!(
            seen.line, expected,
            "only GET and PATCH of the bound Secret"
        );
    }
    assert_eq!(db.secrets.seen().len(), 2, "one GET and one PATCH");

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
    assert_eq!(
        tables, 16,
        "users, memberships, sessions, tenants, login_codes, event_log and platform_event_log, \
         the first platform administrator's marker platform_admin_bootstrap \
         (Laysi/trustiot-v2#582), preferences (Laysi/trustiot-v2#497), \
         the outbox's outbox_aggregate and outbox_event, its consumer tables \
         outbox_subscription, outbox_inbox, outbox_dead_letter and outbox_version (the event log \
         consumes identity's own events, Laysi/trustiot-v2#130), and the migration records \
         trustiot_migrate_applied"
    );
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

    // The event log reads assets' events (identity consumes assets, which is listed after it,
    // Laysi/trustiot-v2#130): identity_platform reads assets.outbox_event, writes nothing there
    // and has nothing else of schema assets.
    let reachable: Vec<String> = sqlx::query_scalar(IDENTITY_PLATFORM_IN_ASSETS)
        .fetch_all(&mut admin)
        .await
        .expect("read identity_platform's rights in assets");
    assert_eq!(
        reachable,
        ["outbox_event"],
        "identity_platform in schema assets"
    );
    let rights: (bool, bool, bool) = sqlx::query_as(IDENTITY_PLATFORM_ON_ASSETS_EVENTS)
        .fetch_one(&mut admin)
        .await
        .expect("read identity_platform's rights on assets.outbox_event");
    assert_eq!(
        rights,
        (true, false, false),
        "identity_platform: SELECT on assets.outbox_event, no write, no CREATE in assets"
    );
    let readers: String = sqlx::query_scalar(ASSETS_EVENT_READERS)
        .fetch_one(&mut admin)
        .await
        .expect("read the assets outbox's read policy");
    let mut readers: Vec<&str> = readers.split(',').collect();
    readers.sort_unstable();
    // The certs service reads assets' events too (Laysi/trustiot-v2#632); assets itself no
    // longer does: it read its own events only until a real consumer was listed. ingest consumes
    // assets.gateway_bound for its gateway binding projection (Laysi/trustiot-v2#640): its entry
    // in src/registry.rs has `consumes: &["assets"]`, and the Job sets this policy to exactly the
    // consumers' platform roles (src/postgres.rs, `consumer_read`), no migration file of its own.
    // gateway-ops consumes assets' model, device and binding events the same way
    // (Laysi/trustiot-v2#719).
    assert_eq!(
        readers,
        [
            "certs_platform",
            "gateway_ops_platform",
            "identity_platform",
            "ingest_platform"
        ]
    );
    let reachable: Vec<String> = sqlx::query_scalar(CERTS_PLATFORM_IN_ASSETS)
        .fetch_all(&mut admin)
        .await
        .expect("read certs_platform's rights in assets");
    assert_eq!(
        reachable,
        ["outbox_event"],
        "certs_platform in schema assets"
    );
    let rights: (bool, bool, bool) = sqlx::query_as(CERTS_PLATFORM_ON_ASSETS_EVENTS)
        .fetch_one(&mut admin)
        .await
        .expect("read certs_platform's rights on assets.outbox_event");
    assert_eq!(
        rights,
        (true, false, false),
        "certs_platform: SELECT on assets.outbox_event, no write, no CREATE in assets"
    );
    // The certs service reads identity's events too (Laysi/trustiot-v2#778), for each tenant's
    // state: its entry in src/registry.rs has `consumes: &["assets", "identity"]`. The other
    // readers are identity's event log, assets (`identity.tenant_created`) and telemetry-query
    // (each tenant's ClickHouse account).
    let readers: String = sqlx::query_scalar(IDENTITY_EVENT_READERS)
        .fetch_one(&mut admin)
        .await
        .expect("read the identity outbox's read policy");
    let mut readers: Vec<&str> = readers.split(',').collect();
    readers.sort_unstable();
    assert_eq!(
        readers,
        [
            "assets_platform",
            "certs_platform",
            "identity_platform",
            "telemetry_query_platform"
        ]
    );
    let reachable: Vec<String> = sqlx::query_scalar(CERTS_PLATFORM_IN_IDENTITY)
        .fetch_all(&mut admin)
        .await
        .expect("read certs_platform's rights in identity");
    assert_eq!(
        reachable,
        ["outbox_event"],
        "certs_platform in schema identity"
    );
    let rights: (bool, bool, bool) = sqlx::query_as(CERTS_PLATFORM_ON_IDENTITY_EVENTS)
        .fetch_one(&mut admin)
        .await
        .expect("read certs_platform's rights on identity.outbox_event");
    assert_eq!(
        rights,
        (true, false, false),
        "certs_platform: SELECT on identity.outbox_event, no write, no CREATE in identity"
    );
    // The consumer's bookkeeping, where its subscription is recorded once the service runs, and
    // the two tenant tables of 0005_gateway_bindings.sql under row-level security.
    let tables: Vec<(String, bool)> = sqlx::query_as(CERTS_CONSUMER_TABLES)
        .fetch_all(&mut admin)
        .await
        .expect("read the certs consumer's tables");
    assert_eq!(
        tables,
        [
            ("gateway_bindings", true),
            ("outbox_dead_letter", false),
            ("outbox_inbox", false),
            ("outbox_subscription", false),
            ("outbox_version", false),
            ("tenant_statuses", true),
        ]
        .map(|(name, rls)| (name.to_owned(), rls)),
        "the certs service's consumer tables and tenant tables"
    );

    // 3. The service role passes service-kit's session check and finds its schema first.
    let mut app_conn = db
        .connect_with_password("identity_app", &first)
        .await
        .expect("identity_app logs in with the password in its Secret");
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

    // 5. Rotation: a new password generation generates a new password and sets it, and only
    //    that; the old one stops working.
    db.set_generation("2");
    let report = migrate::run(&db.config(MIGRATOR), &PRODUCT)
        .await
        .expect("the rotation run");
    assert_eq!(
        report.changes,
        [
            "generated the password of identity_app in Secret identity-app-postgres (password generation 2)",
            "set the password of identity_app"
        ]
    );
    let second = db.password("identity_app");
    assert_ne!(second, first);
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

    // 5b. A lost key (a deleted key, or a Secret that helm upgrade --force-replace emptied) gets
    //     a new password on the next run; a value someone else wrote is what the role gets.
    db.secrets
        .write("identity-app-postgres", "identity_app", None);
    let report = migrate::run(&db.config(MIGRATOR), &PRODUCT)
        .await
        .expect("the run after the key was lost");
    assert_eq!(
        report.changes,
        [
            "generated the password of identity_app in Secret identity-app-postgres (it had none)",
            "set the password of identity_app"
        ]
    );
    let third = db.password("identity_app");
    assert_ne!(third, second);
    db.connect_with_password("identity_app", &third)
        .await
        .expect("the regenerated password logs in");
    let edited = support::random_hex();
    db.secrets.write(
        "identity-app-postgres",
        "identity_app",
        Some(edited.as_bytes()),
    );
    let report = migrate::run(&db.config(MIGRATOR), &PRODUCT)
        .await
        .expect("the run after an edit");
    assert_eq!(report.changes, ["set the password of identity_app"]);
    db.connect_with_password("identity_app", &edited)
        .await
        .expect("the role follows its Secret");
    let second = edited;

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

    // 8. A service role that gained BYPASSRLS, joined the owner role, gained a member other
    //    than the migration role, has settings beyond its search_path (for itself or for this
    //    database), may create in its schema, or may create temporary tables stops the run.
    //    A service role can set its own settings for a database, so it is broken as itself.
    let mut as_app = db
        .connect_with_password("identity_app", &second)
        .await
        .expect("identity_app logs in");
    sqlx::raw_sql(OWN_DATABASE_SEARCH_PATH)
        .execute(&mut as_app)
        .await
        .expect("identity_app sets its own search_path for this database");
    as_app.close().await.expect("close");
    let error = migrate::run(&db.config(MIGRATOR), &PRODUCT)
        .await
        .expect_err("a per-database setting");
    assert!(
        matches!(&error, Error::Role { problem, .. } if problem.contains("settings for this database")),
        "{error}"
    );
    sqlx::raw_sql(RESET_DATABASE_SETTINGS)
        .execute(&mut admin)
        .await
        .expect("an administrator resets them");
    for (break_it, repair, what, expected) in [
        (GIVE_BYPASSRLS, TAKE_BYPASSRLS, "BYPASSRLS", "BYPASSRLS"),
        (
            JOIN_OWNER,
            LEAVE_OWNER,
            "membership in the owner role",
            "only the migration role may be a member of an owner role",
        ),
        (
            ADD_MEMBER,
            REMOVE_MEMBER,
            "a member other than the migration role",
            "another role is a member",
        ),
        (
            SET_TIMEOUT,
            RESET_TIMEOUT,
            "a role setting other than search_path",
            "role settings other than its search_path",
        ),
        (
            GRANT_CREATE,
            REVOKE_CREATE,
            "CREATE on its schema",
            "may create objects",
        ),
        (
            GRANT_TEMPORARY,
            REVOKE_TEMPORARY,
            "TEMPORARY for PUBLIC again",
            "temporary tables",
        ),
    ] {
        sqlx::raw_sql(break_it)
            .execute(&mut admin)
            .await
            .expect("break the role");
        let error = migrate::run(&db.config(MIGRATOR), &PRODUCT)
            .await
            .expect_err(what);
        assert!(
            matches!(&error, Error::Role { problem, .. } if problem.contains(expected)),
            "{what}: {error}"
        );
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
