//! A service file that leaves its transaction or its role stops the run and is not recorded:
//! the Job compares the transaction, `current_user` and the role setting after the file with
//! those before it. A role switch inside the transaction rolls back with it; a file that ended
//! the transaction has committed what it ran, so the next run applies it again and fails closed.
//! The registry lint (`tests/registry.rs`) catches the same statements in product files before
//! they ship; this is what happens when one slips through.
//!
//! Needs podman or docker (a throwaway Postgres with TLS); skipped without one unless `CI` is
//! set. One test only: roles are cluster-wide, and this binary has its own Postgres.

mod support;

use migrate::Error;
use migrate::registry::{Migration, Outbox, PRODUCT, Registry, Service};
use sqlx::Connection;
use support::postgres::tls_postgres;
use support::{Database, MIGRATOR, MIGRATOR_INIT, snapshot};

const CREATE: &str = "CREATE DATABASE migrate_escapes OWNER trustiot_owner";
const ESCAPE_ROWS: &str =
    "SELECT count(*) FROM trustiot_migrate.applied WHERE component = 'escape'";
const PLANTED: &str = "SELECT to_regclass('identity.escape_plant') IS NOT NULL";
const ENDED_FIRST: &str = "SELECT to_regclass('escape.escape_end_first') IS NOT NULL";

/// The product's identity service, then a service `escape` with one file.
macro_rules! with_escape_file {
    ($name:literal, $sql:literal) => {
        Registry {
            services: &[
                PRODUCT.services[0],
                Service {
                    name: "escape",
                    platform_role: false,
                    outbox: Outbox {
                        producer: false,
                        consumes: &[],
                    },
                    migrations: &[Migration {
                        name: $name,
                        sql: $sql,
                    }],
                },
            ],
            clickhouse: &[],
            tenant_role_hooks: &[],
        }
    };
}

/// Leaves its role for the migration role's own (SET LOCAL ROLE NONE).
const ROLE_NONE: Registry = with_escape_file!("0001_role_none.sql", "SET LOCAL ROLE NONE;");
/// Switches to another service's owner role, writes into its schema, and stays there.
const OTHER_OWNER: Registry = with_escape_file!(
    "0001_other_owner.sql",
    "SET LOCAL ROLE identity_owner; CREATE TABLE identity.escape_plant (i int);"
);
/// The same with SET SESSION ROLE, which would outlive the transaction if it committed.
const SESSION_ROLE: Registry = with_escape_file!(
    "0001_session_role.sql",
    "SET SESSION ROLE identity_owner; CREATE TABLE identity.escape_plant (i int);"
);
/// Ends the Job's transaction: what it ran before END is committed.
const ENDS: Registry = with_escape_file!(
    "0001_end.sql",
    "CREATE TABLE escape_end_first (i int); END;"
);
/// Commits, starts the next transaction, and takes its role and schema back: only the
/// transaction differs at the end.
const CHAINS: Registry = with_escape_file!(
    "0001_chain.sql",
    "CREATE TABLE escape_chain_first (i int); COMMIT AND CHAIN; SET LOCAL ROLE escape_owner; SET LOCAL search_path = escape;"
);

#[tokio::test(flavor = "multi_thread")]
async fn a_file_that_leaves_its_transaction_or_role_stops_the_run() {
    let Some(postgres) = tls_postgres(MIGRATOR_INIT).await else {
        return;
    };
    let db = Database::create(postgres, CREATE, "migrate_escapes").await;
    migrate::run(&db.config(MIGRATOR), &PRODUCT)
        .await
        .expect("the product's run");
    let mut admin = sqlx::PgConnection::connect_with(&db.postgres.superuser(&db.name))
        .await
        .expect("connect as the superuser");

    // 1. A role switch left in place stops the run; the transaction rolls back, so nothing the
    //    file did stays and it is not recorded.
    for (registry, what) in [
        (&ROLE_NONE, "SET LOCAL ROLE NONE"),
        (&OTHER_OWNER, "SET LOCAL ROLE identity_owner"),
        (&SESSION_ROLE, "SET SESSION ROLE identity_owner"),
    ] {
        let error = migrate::run(&db.config(MIGRATOR), registry)
            .await
            .expect_err(what);
        assert!(
            matches!(&error, Error::LeftItsContext { file, .. } if file.starts_with("escape/")),
            "{what}: {error}"
        );
        let planted: bool = sqlx::query_scalar(PLANTED)
            .fetch_one(&mut admin)
            .await
            .expect("look for the file's table");
        assert!(!planted, "{what}: identity.escape_plant was rolled back");
        let recorded: i64 = sqlx::query_scalar(ESCAPE_ROWS)
            .fetch_one(&mut admin)
            .await
            .expect("read the bookkeeping");
        assert_eq!(recorded, 0, "{what}: the file was not recorded");
        // The first run of each case creates the escape service's roles and schema; the run
        // stops at the file, so a second attempt leaves the catalogs as the first left them.
        let after_first = snapshot(&db).await;
        migrate::run(&db.config(MIGRATOR), registry)
            .await
            .expect_err(what);
        assert_eq!(
            snapshot(&db).await,
            after_first,
            "{what}: a repeated attempt changed the catalogs"
        );
    }

    // 2. A file that ends the transaction stops the run and is not recorded, but what it ran
    //    before END is committed; the next run applies it again and fails on that, so the run
    //    keeps failing until a fixed file replaces it.
    let error = migrate::run(&db.config(MIGRATOR), &ENDS)
        .await
        .expect_err("END");
    assert!(
        matches!(&error, Error::LeftItsContext { file, .. } if file == "escape/0001_end.sql"),
        "{error}"
    );
    let committed: bool = sqlx::query_scalar(ENDED_FIRST)
        .fetch_one(&mut admin)
        .await
        .expect("look for the table");
    assert!(committed, "what the file ran before END is committed");
    let recorded: i64 = sqlx::query_scalar(ESCAPE_ROWS)
        .fetch_one(&mut admin)
        .await
        .expect("read the bookkeeping");
    assert_eq!(
        recorded, 0,
        "a file that ended the transaction is not recorded"
    );
    let error = migrate::run(&db.config(MIGRATOR), &ENDS)
        .await
        .expect_err("the next run applies the file again");
    assert!(
        matches!(&error, Error::Postgres { source, .. } if source.to_string().contains("already exists")),
        "{error}"
    );

    // 3. A file that commits, chains a new transaction and takes its role back ends as the role
    //    it started as; only the transaction tells, and it stops the run as well.
    let error = migrate::run(&db.config(MIGRATOR), &CHAINS)
        .await
        .expect_err("COMMIT AND CHAIN");
    assert!(
        matches!(&error, Error::LeftItsContext { file, .. } if file == "escape/0001_chain.sql"),
        "{error}"
    );
    let recorded: i64 = sqlx::query_scalar(ESCAPE_ROWS)
        .fetch_one(&mut admin)
        .await
        .expect("read the bookkeeping");
    assert_eq!(
        recorded, 0,
        "a file that chained a transaction is not recorded"
    );
}
