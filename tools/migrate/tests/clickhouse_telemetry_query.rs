//! The telemetry-query service's ClickHouse account (Laysi/trustiot-v2#119): the run gives it
//! the password it generates into the service's Secret and lets it log in only while the
//! service is deployed, the account can create and change trustiot_tenant_account_* accounts and
//! nothing else, and a tampered account, tenant role or tenant role grantee stops the run.
//!
//! Its own binary, because the tests of one binary share one Postgres, which holds one TrustIoT
//! database. Needs podman or docker (a throwaway Postgres for the lock and a throwaway
//! ClickHouse); skipped without one unless `CI` is set.

mod support;

use migrate::Error;
use migrate::config::RunConfig;
use migrate::registry::{PRODUCT, Registry};
use support::postgres::tls_postgres;
use support::{ClickHouse, Database, MIGRATOR, MIGRATOR_INIT};

const CREATE: &str = "CREATE DATABASE migrate_clickhouse_telemetry OWNER trustiot_owner";
const COLLECTOR: &str = "trustiot_collector";
const COLLECTOR_SECRET: &str = "trustiot-otel-collector-password";

fn config(db: &Database, clickhouse: &ClickHouse) -> RunConfig {
    let mut config = db.config(MIGRATOR);
    config.platform.clickhouse_url = Some(clickhouse.url.clone());
    config.clickhouse = Some(clickhouse.settings());
    config.passwords = db.passwords(Some((COLLECTOR, COLLECTOR_SECRET)));
    config
}

/// The product without its Postgres services, so this binary's runs touch ClickHouse and the
/// Job's own bookkeeping only.
const CLICKHOUSE_ONLY: Registry = Registry {
    services: &[],
    clickhouse: PRODUCT.clickhouse,
    tenant_role_hooks: &[],
};

const TELEMETRY_QUERY: &str = "trustiot_telemetry_query";
const TELEMETRY_QUERY_SECRET: &str = "trustiot-telemetry-query-password";

/// Run settings with telemetry-query's account bound to its Secret (the service deployed), or
/// not (`bound` false: the service is off).
fn telemetry_config(db: &Database, clickhouse: &ClickHouse, bound: bool) -> RunConfig {
    let mut config = config(db, clickhouse);
    if bound {
        let passwords = config.passwords.as_mut().expect("password settings");
        passwords.clickhouse_telemetry_query =
            Some(format!("{TELEMETRY_QUERY}={TELEMETRY_QUERY_SECRET}"));
    }
    config
}

fn telemetry_query_password(db: &Database) -> String {
    db.secrets
        .value(TELEMETRY_QUERY_SECRET, TELEMETRY_QUERY)
        .expect("telemetry-query's Secret holds its password")
}

/// Runs `statement` as telemetry-query's account, which ClickHouse must refuse for want of a
/// privilege.
async fn refused_to_telemetry_query(clickhouse: &ClickHouse, password: &str, statement: &str) {
    let error = clickhouse
        .query_as(TELEMETRY_QUERY, password, statement)
        .await
        .expect_err(statement);
    assert!(
        error.contains("ACCESS_DENIED") || error.contains("Not enough privileges"),
        "{statement}: {error}"
    );
}

/// telemetry-query's account (Laysi/trustiot-v2#119): created by 0007_telemetry_query.sql
/// without a password and unable to log in; the run gives it the password the Job generates
/// into its Secret and lets it log in only while the service is deployed; it can create and
/// change accounts named trustiot_tenant_account_* and nothing else; rotation sets the new
/// password; a tampered account or tenant role stops the run.
#[tokio::test(flavor = "multi_thread")]
async fn telemetry_query_account() {
    let Some(postgres) = tls_postgres(MIGRATOR_INIT).await else {
        return;
    };
    let Some(clickhouse) = ClickHouse::start().await else {
        return;
    };
    let db = Database::create(postgres, CREATE, "migrate_clickhouse_telemetry").await;
    db.secrets.declare(COLLECTOR_SECRET);
    db.secrets.declare(TELEMETRY_QUERY_SECRET);
    let hosts = || async {
        clickhouse
            .query(
                "SELECT length(host_ip) + length(host_names) + length(host_names_regexp)
                   + length(host_names_like) FROM system.users
                 WHERE name = 'trustiot_telemetry_query' FORMAT TSV",
            )
            .await
            .expect("read the account's hosts")
    };

    // 1. Without the service the DDL creates the account, and it cannot log in.
    let report = migrate::run(&telemetry_config(&db, &clickhouse, false), &CLICKHOUSE_ONLY)
        .await
        .expect("the run without telemetry-query");
    assert!(
        report
            .changes
            .contains(&"applied clickhouse/0007_telemetry_query.sql".to_owned()),
        "{:?}",
        report.changes
    );
    assert_eq!(
        hosts().await,
        "0\n",
        "HOST NONE until the service is deployed"
    );

    // 2. With the service: the Job generates the password, sets it, and lets the account log in.
    let report = migrate::run(&telemetry_config(&db, &clickhouse, true), &CLICKHOUSE_ONLY)
        .await
        .expect("the run with telemetry-query");
    assert_eq!(
        report.changes,
        [
            format!(
                "generated the password of {TELEMETRY_QUERY} in Secret {TELEMETRY_QUERY_SECRET} \
                 (it had none)"
            ),
            format!("set the password of ClickHouse account {TELEMETRY_QUERY}"),
            format!("let ClickHouse account {TELEMETRY_QUERY} log in"),
        ]
    );
    let first = telemetry_query_password(&db);
    let roles = clickhouse
        .query_as(
            TELEMETRY_QUERY,
            &first,
            "SELECT currentRoles(), getSetting('readonly') FORMAT TSV",
        )
        .await
        .expect("the account logs in with the Secret's password");
    assert_eq!(roles, "[]\t0\n", "no role is active at login");
    let report = migrate::run(&telemetry_config(&db, &clickhouse, true), &CLICKHOUSE_ONLY)
        .await
        .expect("the second run with telemetry-query");
    assert!(report.changes.is_empty(), "{:?}", report.changes);

    // 3. What the account may do: create and change trustiot_tenant_account_* accounts. Nothing
    //    else: no other user name, no other account, no grant, no table without a role.
    clickhouse
        .query_as(
            TELEMETRY_QUERY,
            &first,
            "CREATE USER trustiot_tenant_account_migrate_test IDENTIFIED WITH no_password HOST NONE",
        )
        .await
        .expect("a tenant account name is allowed");
    clickhouse
        .query_as(
            TELEMETRY_QUERY,
            &first,
            "ALTER USER trustiot_tenant_account_migrate_test HOST NONE",
        )
        .await
        .expect("changing a tenant account is allowed");
    for statement in [
        "CREATE USER migrate_test_other IDENTIFIED WITH no_password HOST NONE",
        "CREATE USER trustiot_tenant_accountx IDENTIFIED WITH no_password HOST NONE",
        "ALTER USER trustiot_collector HOST ANY",
        "ALTER USER trustiot_tenant_view_definer HOST ANY",
        "ALTER USER trustiot_telemetry_query SETTINGS readonly = 0",
        "DROP USER trustiot_tenant_account_migrate_test",
        "GRANT trustiot_ingest TO trustiot_tenant_account_migrate_test",
        "GRANT SELECT ON trustiot.telemetry TO trustiot_tenant_account_migrate_test",
        "CREATE ROLE migrate_test_role",
        "SELECT count() FROM trustiot.tenant_telemetry",
        "SELECT count() FROM trustiot.telemetry",
    ] {
        refused_to_telemetry_query(&clickhouse, &first, statement).await;
    }
    clickhouse
        .query("DROP USER trustiot_tenant_account_migrate_test")
        .await
        .expect("clean up the test account");

    // 4. Rotation: a new generation sets a new password, and the old one stops working.
    db.set_generation("2");
    let report = migrate::run(&telemetry_config(&db, &clickhouse, true), &CLICKHOUSE_ONLY)
        .await
        .expect("the rotation run");
    for expected in [
        format!(
            "generated the password of {TELEMETRY_QUERY} in Secret {TELEMETRY_QUERY_SECRET} \
             (password generation 2)"
        ),
        format!("set the password of ClickHouse account {TELEMETRY_QUERY}"),
    ] {
        assert!(report.changes.contains(&expected), "{:?}", report.changes);
    }
    let second = telemetry_query_password(&db);
    assert_ne!(second, first);
    assert!(
        clickhouse
            .query_as(TELEMETRY_QUERY, &first, "SELECT 1")
            .await
            .is_err()
    );
    clickhouse
        .query_as(TELEMETRY_QUERY, &second, "SELECT 1")
        .await
        .expect("the new password logs in");

    // 5. A tampered account, a tenant role with more than SELECT on the tenant view, or a tenant
    //    role held by anyone else than telemetry-query's account (with ADMIN OPTION) and the
    //    tenant accounts (without it) stops the run and is left as it is; once repaired, the run
    //    passes again. telemetry-query's account can itself grant the role to anyone once it
    //    enables it (ClickHouse does not limit the grantee of a role with ADMIN OPTION).
    for statement in [
        "CREATE USER migrate_test_grantee IDENTIFIED WITH no_password HOST NONE",
        "CREATE USER trustiot_tenant_account_migrate_grantee IDENTIFIED WITH no_password HOST NONE",
        "GRANT trustiot_tenant_reader TO trustiot_tenant_account_migrate_grantee",
    ] {
        clickhouse
            .query(statement)
            .await
            .unwrap_or_else(|error| panic!("{statement}: {error}"));
    }
    let report = migrate::run(&telemetry_config(&db, &clickhouse, true), &CLICKHOUSE_ONLY)
        .await
        .expect("a tenant account holding the tenant role is allowed");
    assert!(report.changes.is_empty(), "{:?}", report.changes);
    let granted_by_service = clickhouse
        .query_as_with_role(
            TELEMETRY_QUERY,
            &second,
            "trustiot_tenant_reader",
            "GRANT trustiot_tenant_reader TO migrate_test_grantee",
        )
        .await;
    assert!(
        granted_by_service.is_ok(),
        "ClickHouse lets the holder of ADMIN OPTION grant the role to any user: {granted_by_service:?}"
    );
    let error = migrate::run(&telemetry_config(&db, &clickhouse, true), &CLICKHOUSE_ONLY)
        .await
        .expect_err("a grantee the service made must stop the run");
    assert!(
        matches!(&error, Error::ClickHouse(text) if text.contains("1 grantee(s)") && text.contains("migrate_test_grantee")),
        "{error}"
    );
    clickhouse
        .query("REVOKE trustiot_tenant_reader FROM migrate_test_grantee")
        .await
        .expect("revoke the role");
    for (break_it, repair, expected) in [
        (
            "GRANT SELECT ON trustiot.telemetry TO trustiot_telemetry_query",
            &["REVOKE SELECT ON trustiot.telemetry FROM trustiot_telemetry_query"][..],
            "other roles, grants or settings",
        ),
        (
            "GRANT CREATE USER ON *.* TO trustiot_telemetry_query",
            // A grant on every name absorbs the prefix grant, so the repair grants that again.
            &[
                "REVOKE CREATE USER ON *.* FROM trustiot_telemetry_query",
                "GRANT CREATE USER ON trustiot_tenant_account_* TO trustiot_telemetry_query",
            ][..],
            "other roles, grants or settings",
        ),
        (
            "SET DEFAULT ROLE trustiot_tenant_reader TO trustiot_telemetry_query",
            &["SET DEFAULT ROLE NONE TO trustiot_telemetry_query"][..],
            "other roles, grants or settings",
        ),
        (
            "GRANT trustiot_ingest TO trustiot_telemetry_query",
            &["REVOKE trustiot_ingest FROM trustiot_telemetry_query"][..],
            "other roles, grants or settings",
        ),
        (
            "ALTER USER trustiot_telemetry_query SETTINGS readonly = 0",
            &["ALTER USER trustiot_telemetry_query SETTINGS NONE"][..],
            "other roles, grants or settings",
        ),
        (
            "GRANT SELECT ON trustiot.telemetry TO trustiot_tenant_reader",
            &["REVOKE SELECT ON trustiot.telemetry FROM trustiot_tenant_reader"][..],
            "beyond SELECT on trustiot.tenant_telemetry",
        ),
        (
            "GRANT trustiot_ingest TO trustiot_tenant_reader",
            &["REVOKE trustiot_ingest FROM trustiot_tenant_reader"][..],
            "beyond SELECT on trustiot.tenant_telemetry",
        ),
        (
            "REVOKE SELECT ON trustiot.tenant_telemetry FROM trustiot_tenant_reader",
            &["GRANT SELECT ON trustiot.tenant_telemetry TO trustiot_tenant_reader"][..],
            "lacks SELECT on trustiot.tenant_telemetry",
        ),
        (
            "GRANT trustiot_tenant_reader TO trustiot_ingest",
            &["REVOKE trustiot_tenant_reader FROM trustiot_ingest"][..],
            "role trustiot_ingest",
        ),
        (
            "GRANT trustiot_tenant_reader TO trustiot_tenant_account_migrate_grantee WITH ADMIN OPTION",
            &[
                "REVOKE ADMIN OPTION FOR trustiot_tenant_reader FROM trustiot_tenant_account_migrate_grantee",
            ][..],
            "trustiot_tenant_account_migrate_grantee",
        ),
    ] {
        clickhouse
            .query(break_it)
            .await
            .unwrap_or_else(|error| panic!("{break_it}: {error}"));
        let error = migrate::run(&telemetry_config(&db, &clickhouse, true), &CLICKHOUSE_ONLY)
            .await
            .expect_err(break_it);
        assert!(
            matches!(&error, Error::ClickHouse(text) if text.contains(expected)),
            "{break_it}: {error}"
        );
        for statement in repair {
            clickhouse
                .query(statement)
                .await
                .unwrap_or_else(|error| panic!("{statement}: {error}"));
        }
        let report = migrate::run(&telemetry_config(&db, &clickhouse, true), &CLICKHOUSE_ONLY)
            .await
            .unwrap_or_else(|error| panic!("the run after {repair:?}: {error}"));
        assert!(
            report.changes.is_empty(),
            "{repair:?}: {:?}",
            report.changes
        );
    }

    // 6. The service turned off: the account can no longer log in, and stays (HOST NONE);
    //    turned on again, it logs in with the Secret's password.
    let report = migrate::run(&telemetry_config(&db, &clickhouse, false), &CLICKHOUSE_ONLY)
        .await
        .expect("the run with telemetry-query off");
    assert_eq!(
        report.changes,
        [format!(
            "revoked the login of ClickHouse account {TELEMETRY_QUERY}: telemetry-query is not \
             deployed"
        )]
    );
    assert_eq!(hosts().await, "0\n");
    assert!(
        clickhouse
            .query_as(TELEMETRY_QUERY, &second, "SELECT 1")
            .await
            .is_err(),
        "the account cannot log in while the service is off"
    );
    let report = migrate::run(&telemetry_config(&db, &clickhouse, true), &CLICKHOUSE_ONLY)
        .await
        .expect("the run with telemetry-query on again");
    assert_eq!(
        report.changes,
        [
            format!("set the password of ClickHouse account {TELEMETRY_QUERY}"),
            format!("let ClickHouse account {TELEMETRY_QUERY} log in"),
        ]
    );
    clickhouse
        .query_as(TELEMETRY_QUERY, &second, "SELECT 1")
        .await
        .expect("the account logs in again");
}
