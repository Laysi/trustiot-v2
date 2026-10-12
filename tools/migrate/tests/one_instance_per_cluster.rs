//! One Postgres cluster holds one TrustIoT database: roles are cluster-wide and the Job's role
//! names carry no instance name, so a second instance on the same cluster would share the first
//! one's service roles, passwords and grants. The Job refuses both ways it could happen: a
//! migration role that owns a second database, and a second migration role that was given the
//! first instance's roles with ADMIN OPTION. Either way the first instance is left as it was.
//!
//! Needs podman or docker (a throwaway Postgres with TLS); skipped without one unless `CI` is
//! set. One test only: roles are cluster-wide, and this binary has its own Postgres.

mod support;

use migrate::Error;
use migrate::registry::PRODUCT;
use support::postgres::tls_postgres;
use support::{Database, MIGRATOR, MIGRATOR_INIT, random_hex, snapshot, superuser};

const CREATE_A: &str = "CREATE DATABASE migrate_instance_a OWNER trustiot_owner";
const CREATE_B: &str = "CREATE DATABASE migrate_instance_b OWNER trustiot_owner";
const DROP_B: &str = "DROP DATABASE migrate_instance_b WITH (FORCE)";
/// A second instance's migration role, made as CloudNativePG or an administrator would make it
/// (the shared test password of the container).
const SECOND_MIGRATOR: &str = "trustiot_owner_b";
const CREATE_SECOND_MIGRATOR: &str =
    "CREATE ROLE trustiot_owner_b LOGIN CREATEROLE PASSWORD 'service-kit-test'";
const CREATE_C: &str = "CREATE DATABASE migrate_instance_c OWNER trustiot_owner_b";
/// What the runbook says never to do: share the first instance's roles with the second.
const SHARE_ROLES: &str =
    "GRANT identity_owner, identity_app TO trustiot_owner_b WITH ADMIN OPTION";
const BOOKKEEPING_EXISTS: &str = "SELECT to_regclass('trustiot_migrate.applied') IS NOT NULL";

#[tokio::test(flavor = "multi_thread")]
async fn a_second_instance_on_the_same_cluster_is_refused() {
    let Some(postgres) = tls_postgres(MIGRATOR_INIT).await else {
        return;
    };
    let a = Database::create(postgres.clone(), CREATE_A, "migrate_instance_a").await;
    let password_a = random_hex();
    a.set_password_file("identity_app", &password_a);
    migrate::run(&a.config(MIGRATOR), &PRODUCT)
        .await
        .expect("the first instance's run");
    let before = snapshot(&a).await;

    // 1. The same migration role owns a second database: the run on it stops before it creates
    //    anything, since it would reuse (and re-password) the first instance's roles.
    let b = Database::create(postgres.clone(), CREATE_B, "migrate_instance_b").await;
    b.set_password_file("identity_app", &random_hex());
    let error = migrate::run(&b.config(MIGRATOR), &PRODUCT)
        .await
        .expect_err("a migration role that owns two databases must be refused");
    assert!(
        matches!(&error, Error::MigrationRole { problem, .. }
            if problem.contains("more than one database")),
        "{error}"
    );
    let mut on_b = b.connect(MIGRATOR).await;
    let created: bool = sqlx::query_scalar(BOOKKEEPING_EXISTS)
        .fetch_one(&mut on_b)
        .await
        .expect("look for the bookkeeping table");
    assert!(
        !created,
        "the refused run created nothing in the second database"
    );
    drop(on_b);
    assert_eq!(snapshot(&a).await, before, "the first instance changed");
    a.connect_with_password("identity_app", &password_a)
        .await
        .expect("the first instance's service role still logs in with its password");

    // 2. A second migration role with its own database, given the first instance's roles with
    //    ADMIN OPTION: the roles own objects and hold privileges in the first database, so the
    //    run stops before it changes them.
    let mut admin = superuser(&postgres).await;
    for statement in [DROP_B, CREATE_SECOND_MIGRATOR] {
        sqlx::raw_sql(statement)
            .execute(&mut admin)
            .await
            .unwrap_or_else(|error| panic!("{statement}: {error}"));
    }
    let c = Database::create(postgres.clone(), CREATE_C, "migrate_instance_c").await;
    sqlx::raw_sql(SHARE_ROLES)
        .execute(&mut admin)
        .await
        .expect("share the roles");
    let before = snapshot(&a).await;
    c.set_password_file("identity_app", &random_hex());
    let error = migrate::run(&c.config(SECOND_MIGRATOR), &PRODUCT)
        .await
        .expect_err("roles of another database must be refused");
    assert!(
        matches!(&error, Error::Role { problem, .. } if problem.contains("another database")),
        "{error}"
    );
    assert_eq!(snapshot(&a).await, before, "the first instance changed");
    a.connect_with_password("identity_app", &password_a)
        .await
        .expect("the first instance's service role still logs in with its password");
}
