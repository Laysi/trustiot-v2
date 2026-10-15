//! The service roles stay out of the cluster's other databases (Laysi/trustiot-v2#400): on
//! PostgreSQL 18 a role the Job creates can connect to `postgres` and `template1` and create
//! temporary tables in `postgres` until PUBLIC loses CONNECT and TEMPORARY there. The bundled
//! Cluster runs the statements of `deploy/chart/files/postgres/system-databases.sql` at bootstrap,
//! one per item of its postInitSQL (the chart test checks the template carries exactly these), and
//! the runbook runs the same file on an existing Cluster; this test runs it one statement at a
//! time, split as the chart test splits it. The
//! Job warns, without stopping, while another database still admits the service roles.
//!
//! Needs podman or docker (a throwaway Postgres with TLS); skipped without one unless `CI` is
//! set. One test only: roles are cluster-wide, and this binary has its own Postgres.

mod support;

use migrate::registry::PRODUCT;
use sqlx::{Connection, PgConnection};
use support::postgres::tls_postgres;
use support::{Database, MIGRATOR, MIGRATOR_INIT, superuser};

const CREATE: &str = "CREATE DATABASE migrate_system_databases OWNER trustiot_owner";

/// The statements of the bundled Cluster's postInitSQL (templates/postgres.yaml).
const SYSTEM_DATABASES: &str =
    include_str!("../../../deploy/chart/files/postgres/system-databases.sql");

/// The cluster's other databases that take connections after initdb.
const OTHER_DATABASES: [&str; 2] = ["postgres", "template1"];

const CREATE_TEMPORARY_TABLE: &str = "CREATE TEMPORARY TABLE scratch (x int)";
const CREATE_IN_PUBLIC: &str = "CREATE TABLE public.left_behind (x int)";
const PRIVILEGES: &str = "SELECT has_database_privilege($1, $2, 'CONNECT'),
    has_database_privilege($1, $2, 'TEMPORARY')";
const DATABASE_ACLS: &str = "SELECT format('%s %s', datname, datacl) FROM pg_database ORDER BY 1";
const ROLE_SHAPE: &str = "SELECT format('%s login=%s replication=%s super=%s inherit=%s',
    rolname, rolcanlogin, rolreplication, rolsuper, rolinherit) FROM pg_roles WHERE rolname = $1";
/// A database created from template1 after the revocation, as CloudNativePG creates the TrustIoT
/// database right after postInitSQL.
const CREATE_AFTER: &str = "CREATE DATABASE migrate_created_after";
const DROP_AFTER: &str = "DROP DATABASE migrate_created_after";
const READ_IN_OWN_DATABASE: &str = "SELECT current_database()::text";

/// The statements of the SQL file, split as the chart test splits them: blocks separated by a line
/// that holds nothing but spaces or tabs, and lines that start with `--` are comments. Each block
/// is a slice of the file, comments included (Postgres ignores them), so it is a `&'static str`;
/// a block that holds only comments is no statement.
fn statements(text: &'static str) -> Vec<&'static str> {
    let mut blocks = Vec::new();
    let mut start = 0;
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        if line.trim_matches([' ', '\t', '\n']).is_empty() {
            blocks.push(slice(text, start, offset));
            start = offset + line.len();
        }
        offset += line.len();
    }
    blocks.push(slice(text, start, text.len()));
    blocks
        .into_iter()
        .filter(|block| !without_comments(block).is_empty())
        .collect()
}

/// `text[start..end]`, where both ends are line boundaries (after a `\n`), so character
/// boundaries too.
fn slice(text: &'static str, start: usize, end: usize) -> &'static str {
    text.get(start..end)
        .expect("line boundaries are character boundaries")
}

/// A block's text as the chart renders it: comment lines removed, trimmed.
fn without_comments(block: &str) -> String {
    block
        .lines()
        .map(|line| if line.starts_with("--") { "" } else { line })
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_owned()
}

/// The SQLSTATE of a refused connection or statement, or `None` when it was not refused.
fn refusal<T>(result: Result<T, sqlx::Error>) -> Option<String> {
    match result {
        Ok(_) => None,
        Err(sqlx::Error::Database(error)) => Some(error.code().unwrap_or_default().into_owned()),
        Err(error) => panic!("expected a refusal from Postgres, got {error}"),
    }
}

async fn connect_to(
    db: &Database,
    role: &str,
    password: &str,
    database: &str,
) -> Result<PgConnection, sqlx::Error> {
    PgConnection::connect_with(&db.postgres.options(role, database).password(password)).await
}

async fn privileges(admin: &mut PgConnection, role: &str, database: &str) -> (bool, bool) {
    sqlx::query_as(PRIVILEGES)
        .bind(role)
        .bind(database)
        .fetch_one(&mut *admin)
        .await
        .expect("read the database privileges")
}

#[tokio::test(flavor = "multi_thread")]
async fn service_roles_cannot_use_the_other_databases() {
    let Some(postgres) = tls_postgres(MIGRATOR_INIT).await else {
        return;
    };
    let db = Database::create(postgres.clone(), CREATE, "migrate_system_databases").await;
    db.bind("identity_app");
    let login_roles: Vec<String> = PRODUCT
        .services
        .iter()
        .flat_map(migrate::registry::Service::login_roles)
        .collect();

    // 0. Every statement of the file ends with `;` before the next blank line.
    let split = statements(SYSTEM_DATABASES);
    assert!(!split.is_empty(), "the SQL file holds no statement");
    for statement in split.iter().copied() {
        assert!(
            without_comments(statement).ends_with(';'),
            "a statement must end with ; before the next blank line: {statement}"
        );
    }

    // 1. Before the file, as on an external Postgres nobody changed: the Job creates the roles
    //    and warns about both databases, but does not stop. PostgreSQL 18 lets the service role
    //    connect to both and create a temporary table in `postgres`; template1 gives PUBLIC
    //    CONNECT only, and PUBLIC may create nothing in `public` of either (PostgreSQL 15 on).
    let report = migrate::run(&db.config(MIGRATOR), &PRODUCT)
        .await
        .expect("the first run");
    // Sorted by name, as the Job lists them.
    let mut sorted = login_roles.clone();
    sorted.sort();
    let roles = sorted.join(", ");
    assert_eq!(
        report.warnings,
        vec![
            format!(
                "database postgres: service roles {roles} may connect to it and create \
                 temporary tables there; revoke CONNECT and TEMPORARY on it from PUBLIC as a \
                 superuser (docs/runbooks/migrations.md)"
            ),
            format!(
                "database template1: service roles {roles} may connect to it; revoke CONNECT \
                 and TEMPORARY on it from PUBLIC as a superuser (docs/runbooks/migrations.md)"
            ),
        ],
        "the warnings name each database and every service role, sorted"
    );
    let password = db.password("identity_app");
    let mut on_postgres = connect_to(&db, "identity_app", &password, "postgres")
        .await
        .expect("before the file, the service role connects to postgres");
    sqlx::raw_sql(CREATE_TEMPORARY_TABLE)
        .execute(&mut on_postgres)
        .await
        .expect("before the file, the service role creates a temporary table in postgres");
    assert_eq!(
        refusal(
            sqlx::raw_sql(CREATE_IN_PUBLIC)
                .execute(&mut on_postgres)
                .await
        )
        .as_deref(),
        Some("42501"),
        "PUBLIC creates nothing in public of postgres"
    );
    on_postgres.close().await.expect("close");
    let mut on_template = connect_to(&db, "identity_app", &password, "template1")
        .await
        .expect("before the file, the service role connects to template1");
    assert_eq!(
        refusal(
            sqlx::raw_sql(CREATE_TEMPORARY_TABLE)
                .execute(&mut on_template)
                .await
        )
        .as_deref(),
        Some("42501"),
        "initdb gives PUBLIC no TEMPORARY on template1"
    );
    assert_eq!(
        refusal(
            sqlx::raw_sql(CREATE_IN_PUBLIC)
                .execute(&mut on_template)
                .await
        )
        .as_deref(),
        Some("42501"),
        "PUBLIC creates nothing in public of template1"
    );
    on_template.close().await.expect("close");

    // 2. The file, as the superuser on `postgres` (CloudNativePG's postInitSQL, or the runbook on
    //    an existing Cluster), one statement at a time; twice, since the runbook may run it again.
    let mut admin = superuser(&postgres).await;
    for round in ["first", "second"] {
        for statement in split.iter().copied() {
            sqlx::raw_sql(statement)
                .execute(&mut admin)
                .await
                .unwrap_or_else(|error| panic!("{round} run of {statement}: {error}"));
        }
    }
    let acls: Vec<String> = sqlx::query_scalar(DATABASE_ACLS)
        .fetch_all(&mut admin)
        .await
        .expect("read the database ACLs");
    assert!(
        acls.contains(
            &"postgres {postgres=CTc/postgres,streaming_replica=c/postgres,cnpg_metrics_exporter=c/postgres}"
                .to_owned()
        ),
        "postgres: only the superuser and CloudNativePG's two roles, by name: {acls:?}"
    );
    assert!(
        acls.contains(&"template1 {postgres=CTc/postgres}".to_owned()),
        "template1: the superuser only: {acls:?}"
    );
    // CloudNativePG's roles, created as the operator creates them (it keeps a role that exists).
    for (role, shape) in [
        (
            "streaming_replica",
            "streaming_replica login=t replication=t super=f inherit=t",
        ),
        (
            "cnpg_metrics_exporter",
            "cnpg_metrics_exporter login=t replication=f super=f inherit=t",
        ),
    ] {
        let found: String = sqlx::query_scalar(ROLE_SHAPE)
            .bind(role)
            .fetch_one(&mut admin)
            .await
            .unwrap_or_else(|error| panic!("read role {role}: {error}"));
        assert_eq!(found, shape);
        assert_eq!(
            privileges(&mut admin, role, "postgres").await,
            (true, false),
            "{role} may connect to postgres and create no temporary table"
        );
        assert_eq!(
            privileges(&mut admin, role, "template1").await,
            (false, false),
            "{role} has nothing on template1"
        );
    }
    // A database created from template1 afterwards gets the default privileges, not
    // template1's: the TrustIoT database, created right after postInitSQL, keeps admitting
    // the service roles.
    sqlx::raw_sql(CREATE_AFTER)
        .execute(&mut admin)
        .await
        .expect("create a database after the file");
    assert!(
        privileges(&mut admin, "identity_app", "migrate_created_after")
            .await
            .0,
        "a database created from template1 does not copy template1's privileges"
    );
    sqlx::raw_sql(DROP_AFTER)
        .execute(&mut admin)
        .await
        .expect("drop it again");

    // 3. The Job again: the service roles have nothing on the two databases, the run warns about
    //    nothing and changes nothing, and the TrustIoT database keeps working.
    let report = migrate::run(&db.config(MIGRATOR), &PRODUCT)
        .await
        .expect("the run after the file");
    assert_eq!(report.warnings, Vec::<String>::new());
    assert_eq!(report.changes, Vec::<String>::new());
    for role in login_roles.iter().map(String::as_str).chain([MIGRATOR]) {
        for database in OTHER_DATABASES {
            assert_eq!(
                privileges(&mut admin, role, database).await,
                (false, false),
                "{role} has neither CONNECT nor TEMPORARY on {database}"
            );
        }
    }
    for database in OTHER_DATABASES {
        let refused = refusal(connect_to(&db, "identity_app", &password, database).await);
        assert_eq!(
            refused.as_deref(),
            Some("42501"),
            "the service role cannot connect to {database}, so it creates nothing there"
        );
    }
    let mut own = db
        .connect_with_password("identity_app", &password)
        .await
        .expect("the service role still connects to the TrustIoT database");
    let name: String = sqlx::query_scalar(READ_IN_OWN_DATABASE)
        .fetch_one(&mut own)
        .await
        .expect("query the TrustIoT database");
    assert_eq!(name, "migrate_system_databases");
    own.close().await.expect("close");
    admin.close().await.expect("close");
}

#[test]
fn the_split_matches_the_chart_tests_rule() {
    // Blank lines (also with spaces or tabs) separate statements, `--` lines are comments, and a
    // block of comments only is no statement.
    let text: &'static str =
        "-- header\n\nSELECT 1;\n \t\n-- note\nDO $$\nBEGIN\n    -- inner\nEND\n$$;\n\n-- only\n";
    let split = statements(text);
    assert_eq!(split.len(), 2, "{split:?}");
    assert_eq!(without_comments(split[0]), "SELECT 1;");
    assert_eq!(
        without_comments(split[1]),
        "DO $$\nBEGIN\n    -- inner\nEND\n$$;"
    );
}
