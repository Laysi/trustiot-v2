//! The ingest service's ClickHouse account (Laysi/trustiot-v2#642): the run creates it with the
//! role trustiot_ingest alone and the password it generates into the service's Secret, lets it
//! log in only while the service is deployed, and a tampered account, ingest role or ingest role
//! holder revokes the account's login and stops the run.
//!
//! Its own binary, because the tests of one binary share one Postgres, which holds one TrustIoT
//! database. Needs podman or docker (a throwaway Postgres for the lock and a throwaway
//! ClickHouse); skipped without one unless `CI` is set.

mod support;

use migrate::Error;
use migrate::config::{INGEST_ACCOUNT, RunConfig};
use migrate::registry::{PRODUCT, Registry};
use support::postgres::tls_postgres;
use support::{ClickHouse, Database, MIGRATOR, MIGRATOR_INIT};

const CREATE: &str = "CREATE DATABASE migrate_clickhouse_ingest OWNER trustiot_owner";
const COLLECTOR: &str = "trustiot_collector";
const COLLECTOR_SECRET: &str = "trustiot-otel-collector-password";
const INGEST_SECRET: &str = "trustiot-ingest-password";

/// The product without its Postgres services, so this binary's runs touch ClickHouse and the
/// Job's own bookkeeping only.
const CLICKHOUSE_ONLY: Registry = Registry {
    services: &[],
    clickhouse: PRODUCT.clickhouse,
    tenant_role_hooks: &[],
};

/// Run settings with ingest's account bound to its Secret (the service deployed), or not
/// (`bound` false: the service is off).
fn ingest_config(db: &Database, clickhouse: &ClickHouse, bound: bool) -> RunConfig {
    let mut config = db.config(MIGRATOR);
    config.platform.clickhouse_url = Some(clickhouse.url.clone());
    config.clickhouse = Some(clickhouse.settings());
    config.passwords = db.passwords(Some((COLLECTOR, COLLECTOR_SECRET)));
    if bound {
        let passwords = config.passwords.as_mut().expect("password settings");
        passwords.clickhouse_ingest = Some(format!("{INGEST_ACCOUNT}={INGEST_SECRET}"));
    }
    config
}

fn ingest_password(db: &Database) -> String {
    db.secrets
        .value(INGEST_SECRET, INGEST_ACCOUNT)
        .expect("ingest's Secret holds its password")
}

/// Runs `statement` as ingest's account, which ClickHouse must refuse for want of a privilege.
async fn refused_to_ingest(clickhouse: &ClickHouse, password: &str, statement: &str) {
    let error = clickhouse
        .query_as(INGEST_ACCOUNT, password, statement)
        .await
        .expect_err(statement);
    assert!(
        error.contains("ACCESS_DENIED") || error.contains("Not enough privileges"),
        "{statement}: {error}"
    );
}

/// ingest's account (Laysi/trustiot-v2#642): none until the service is deployed; then the run
/// creates it with trustiot_ingest as its only, default role and the password the Job generates
/// into its Secret; it reads and writes the telemetry tables ingest writes and nothing else;
/// rotation sets the new password; a tampered account, ingest role or ingest role holder revokes
/// the account's login and stops the run; with the service off the account cannot log in.
#[tokio::test(flavor = "multi_thread")]
async fn ingest_account() {
    let Some(postgres) = tls_postgres(MIGRATOR_INIT).await else {
        return;
    };
    let Some(clickhouse) = ClickHouse::start().await else {
        return;
    };
    let db = Database::create(postgres, CREATE, "migrate_clickhouse_ingest").await;
    db.secrets.declare(COLLECTOR_SECRET);
    db.secrets.declare(INGEST_SECRET);
    let hosts = || async {
        clickhouse
            .query_with(
                "SELECT length(host_ip) + length(host_names) + length(host_names_regexp)
                   + length(host_names_like) FROM system.users
                 WHERE name = {user:String} FORMAT TSV",
                &[("user", INGEST_ACCOUNT)],
            )
            .await
            .expect("read the account's hosts")
    };

    // 1. Without the service no account is made.
    migrate::run(&ingest_config(&db, &clickhouse, false), &CLICKHOUSE_ONLY)
        .await
        .expect("the run without ingest");
    assert_eq!(hosts().await, "", "no account while ingest is not deployed");

    // 2. With the service: the Job generates the password and creates the account with it.
    let report = migrate::run(&ingest_config(&db, &clickhouse, true), &CLICKHOUSE_ONLY)
        .await
        .expect("the run with ingest");
    assert_eq!(
        report.changes,
        [
            format!(
                "generated the password of {INGEST_ACCOUNT} in Secret {INGEST_SECRET} (it had \
                 none)"
            ),
            format!("created ClickHouse account {INGEST_ACCOUNT}"),
        ]
    );
    let first = ingest_password(&db);
    let roles = clickhouse
        .query_as(
            INGEST_ACCOUNT,
            &first,
            "SELECT currentRoles(), getSetting('readonly') FORMAT TSV",
        )
        .await
        .expect("the account logs in with the Secret's password");
    assert_eq!(
        roles, "['trustiot_ingest']\t0\n",
        "trustiot_ingest is its one role, active at login"
    );
    let report = migrate::run(&ingest_config(&db, &clickhouse, true), &CLICKHOUSE_ONLY)
        .await
        .expect("the second run with ingest");
    assert!(report.changes.is_empty(), "{:?}", report.changes);

    // 3. What the account may do: read and write the tables ingest writes (services/ingest/README.md,
    //    「寫入流程」). Nothing else: no other table or view, no account, no grant, no DDL, and
    //    nothing of the retired completion table.
    for statement in [
        "SELECT count() FROM trustiot.telemetry",
        "SELECT count() FROM trustiot.stream_commits",
        "SELECT count() FROM trustiot.telemetry_1m",
        "SELECT count() FROM trustiot.telemetry_1h",
    ] {
        clickhouse
            .query_as(INGEST_ACCOUNT, &first, statement)
            .await
            .unwrap_or_else(|error| panic!("{statement}: {error}"));
    }
    for statement in [
        "SELECT count() FROM trustiot.batch_commits",
        "SELECT count() FROM trustiot.tenant_telemetry",
        "SELECT count() FROM trustiot_logs.otel_logs",
        "SELECT count() FROM trustiot_migrate.applied",
        "ALTER TABLE trustiot.telemetry MODIFY SETTING ttl_only_drop_parts = 1",
        "ALTER TABLE trustiot.stream_commits DELETE WHERE 1",
        "DROP TABLE trustiot.telemetry",
        "TRUNCATE TABLE trustiot.telemetry",
        "CREATE USER migrate_test_ingest_made IDENTIFIED WITH no_password HOST NONE",
        "ALTER USER trustiot_ingest_writer HOST ANY",
        "GRANT trustiot_tenant_reader TO trustiot_ingest_writer",
        "CREATE ROLE migrate_test_ingest_role",
    ] {
        refused_to_ingest(&clickhouse, &first, statement).await;
    }

    // 4. Rotation: a new generation sets a new password, and the old one stops working.
    db.set_generation("2");
    let report = migrate::run(&ingest_config(&db, &clickhouse, true), &CLICKHOUSE_ONLY)
        .await
        .expect("the rotation run");
    for expected in [
        format!(
            "generated the password of {INGEST_ACCOUNT} in Secret {INGEST_SECRET} (password \
             generation 2)"
        ),
        format!("set the password of ClickHouse account {INGEST_ACCOUNT}"),
    ] {
        assert!(report.changes.contains(&expected), "{:?}", report.changes);
    }
    let second = ingest_password(&db);
    assert_ne!(second, first);
    assert!(
        clickhouse
            .query_as(INGEST_ACCOUNT, &first, "SELECT 1")
            .await
            .is_err()
    );
    clickhouse
        .query_as(INGEST_ACCOUNT, &second, "SELECT 1")
        .await
        .expect("the new password logs in");

    // 5. A tampered account, an ingest role with more or less than the DDL files gave it, or an
    //    ingest role held by anyone else revokes the account's login (HOST NONE) and stops the
    //    run; once repaired, the run sets the password and lets the account log in again. The
    //    stand-ins are a user and roles of their own: the tenant role's holders and roles are
    //    telemetry-query's check, which would stop the run first.
    for statement in [
        "CREATE USER migrate_test_ingest_holder IDENTIFIED WITH no_password HOST NONE",
        "CREATE ROLE migrate_test_ingest_extra",
        "CREATE ROLE migrate_test_ingest_holder_role",
    ] {
        clickhouse
            .query(statement)
            .await
            .unwrap_or_else(|error| panic!("{statement}: {error}"));
    }
    for (break_it, repair, expected) in [
        (
            "GRANT SELECT ON trustiot.tenant_telemetry TO trustiot_ingest_writer",
            &["REVOKE SELECT ON trustiot.tenant_telemetry FROM trustiot_ingest_writer"][..],
            "other roles, grants or settings than trustiot_ingest",
        ),
        (
            "GRANT migrate_test_ingest_extra TO trustiot_ingest_writer",
            &["REVOKE migrate_test_ingest_extra FROM trustiot_ingest_writer"][..],
            "other roles, grants or settings than trustiot_ingest",
        ),
        (
            "SET DEFAULT ROLE NONE TO trustiot_ingest_writer",
            &["SET DEFAULT ROLE trustiot_ingest TO trustiot_ingest_writer"][..],
            "other roles, grants or settings than trustiot_ingest",
        ),
        (
            "GRANT trustiot_ingest TO trustiot_ingest_writer WITH ADMIN OPTION",
            &["REVOKE ADMIN OPTION FOR trustiot_ingest FROM trustiot_ingest_writer"][..],
            "other roles, grants or settings than trustiot_ingest",
        ),
        (
            "ALTER USER trustiot_ingest_writer SETTINGS readonly = 0",
            &["ALTER USER trustiot_ingest_writer SETTINGS NONE"][..],
            "other roles, grants or settings than trustiot_ingest",
        ),
        (
            "GRANT SELECT ON trustiot.tenant_telemetry TO trustiot_ingest",
            &["REVOKE SELECT ON trustiot.tenant_telemetry FROM trustiot_ingest"][..],
            "the role trustiot_ingest has other grants or roles",
        ),
        (
            "GRANT SELECT ON trustiot.batch_commits TO trustiot_ingest",
            &["REVOKE SELECT ON trustiot.batch_commits FROM trustiot_ingest"][..],
            "the role trustiot_ingest has other grants or roles",
        ),
        (
            "GRANT ALTER UPDATE ON trustiot.telemetry TO trustiot_ingest",
            &["REVOKE ALTER UPDATE ON trustiot.telemetry FROM trustiot_ingest"][..],
            "the role trustiot_ingest has other grants or roles",
        ),
        (
            "GRANT SELECT ON trustiot.telemetry TO trustiot_ingest WITH GRANT OPTION",
            &["REVOKE GRANT OPTION FOR SELECT ON trustiot.telemetry FROM trustiot_ingest"][..],
            "the role trustiot_ingest has other grants or roles",
        ),
        (
            "REVOKE ALTER DELETE ON trustiot.telemetry FROM trustiot_ingest",
            &["GRANT ALTER DELETE ON trustiot.telemetry TO trustiot_ingest"][..],
            "the role trustiot_ingest has other grants or roles",
        ),
        (
            "GRANT migrate_test_ingest_extra TO trustiot_ingest",
            &["REVOKE migrate_test_ingest_extra FROM trustiot_ingest"][..],
            "the role trustiot_ingest has other grants or roles",
        ),
        (
            "GRANT trustiot_ingest TO migrate_test_ingest_holder",
            &["REVOKE trustiot_ingest FROM migrate_test_ingest_holder"][..],
            "held by 1 grantee(s) besides trustiot_ingest_writer: migrate_test_ingest_holder",
        ),
        (
            "GRANT trustiot_ingest TO migrate_test_ingest_holder_role",
            &["REVOKE trustiot_ingest FROM migrate_test_ingest_holder_role"][..],
            "besides trustiot_ingest_writer: role migrate_test_ingest_holder_role",
        ),
    ] {
        clickhouse
            .query(break_it)
            .await
            .unwrap_or_else(|error| panic!("{break_it}: {error}"));
        let error = migrate::run(&ingest_config(&db, &clickhouse, true), &CLICKHOUSE_ONLY)
            .await
            .expect_err(break_it);
        assert!(
            matches!(&error, Error::ClickHouse(text) if text.contains(expected)
                && text.contains("revoked the login of trustiot_ingest_writer")),
            "{break_it}: {error}"
        );
        assert_eq!(hosts().await, "0\n", "{break_it}: the login is revoked");
        assert!(
            clickhouse
                .query_as(INGEST_ACCOUNT, &second, "SELECT 1")
                .await
                .is_err(),
            "{break_it}: the account cannot log in"
        );
        for statement in repair {
            clickhouse
                .query(statement)
                .await
                .unwrap_or_else(|error| panic!("{statement}: {error}"));
        }
        let report = migrate::run(&ingest_config(&db, &clickhouse, true), &CLICKHOUSE_ONLY)
            .await
            .unwrap_or_else(|error| panic!("the run after {repair:?}: {error}"));
        assert_eq!(
            report.changes,
            [
                format!("set the password of ClickHouse account {INGEST_ACCOUNT}"),
                format!("let ClickHouse account {INGEST_ACCOUNT} log in"),
            ],
            "{repair:?}"
        );
        clickhouse
            .query_as(INGEST_ACCOUNT, &second, "SELECT 1")
            .await
            .unwrap_or_else(|error| panic!("logs in again after {repair:?}: {error}"));
    }

    // 6. The service turned off: the account can no longer log in, and stays (HOST NONE); turned
    //    on again, it logs in with the Secret's password.
    let report = migrate::run(&ingest_config(&db, &clickhouse, false), &CLICKHOUSE_ONLY)
        .await
        .expect("the run with ingest off");
    assert_eq!(
        report.changes,
        [format!(
            "revoked the login of ClickHouse account {INGEST_ACCOUNT}: ingest is not deployed"
        )]
    );
    assert_eq!(hosts().await, "0\n");
    assert!(
        clickhouse
            .query_as(INGEST_ACCOUNT, &second, "SELECT 1")
            .await
            .is_err(),
        "the account cannot log in while the service is off"
    );
    let report = migrate::run(&ingest_config(&db, &clickhouse, false), &CLICKHOUSE_ONLY)
        .await
        .expect("the second run with ingest off");
    assert!(report.changes.is_empty(), "{:?}", report.changes);
    let report = migrate::run(&ingest_config(&db, &clickhouse, true), &CLICKHOUSE_ONLY)
        .await
        .expect("the run with ingest on again");
    assert_eq!(
        report.changes,
        [
            format!("set the password of ClickHouse account {INGEST_ACCOUNT}"),
            format!("let ClickHouse account {INGEST_ACCOUNT} log in"),
        ]
    );
    clickhouse
        .query_as(INGEST_ACCOUNT, &second, "SELECT 1")
        .await
        .expect("the account logs in again");

    // 7. Before the account exists, another holder of the ingest role stops the run, and no
    //    account is made for it.
    clickhouse
        .query("DROP USER trustiot_ingest_writer")
        .await
        .expect("drop the account");
    clickhouse
        .query("GRANT trustiot_ingest TO migrate_test_ingest_holder")
        .await
        .expect("grant the role to the stand-in");
    let error = migrate::run(&ingest_config(&db, &clickhouse, true), &CLICKHOUSE_ONLY)
        .await
        .expect_err("another holder before the account exists");
    assert!(
        matches!(&error, Error::ClickHouse(text) if text.contains("migrate_test_ingest_holder")
            && !text.contains("revoked the login")),
        "{error}"
    );
    assert_eq!(hosts().await, "", "no account is made");
    clickhouse
        .query("REVOKE trustiot_ingest FROM migrate_test_ingest_holder")
        .await
        .expect("revoke the role from the stand-in");
    let report = migrate::run(&ingest_config(&db, &clickhouse, true), &CLICKHOUSE_ONLY)
        .await
        .expect("the run after the repair");
    assert_eq!(
        report.changes,
        [format!("created ClickHouse account {INGEST_ACCOUNT}")]
    );
}
