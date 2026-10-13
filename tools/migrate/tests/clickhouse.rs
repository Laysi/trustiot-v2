//! The ClickHouse part of a run: the product's DDL on a fresh ClickHouse, the collector's
//! account (created with only its role and the password the run generates into its Secret,
//! password rotation, a tampered account or collector role refused), and a second run that
//! changes nothing.
//!
//! Needs podman or docker (a throwaway Postgres for the lock and a throwaway ClickHouse);
//! skipped without one unless `CI` is set.

mod support;

use migrate::Error;
use migrate::config::RunConfig;
use migrate::registry::{Migration, PRODUCT, Registry};
use support::postgres::tls_postgres;
use support::{ClickHouse, Database, MIGRATOR, MIGRATOR_INIT};

const CREATE: &str = "CREATE DATABASE migrate_clickhouse OWNER trustiot_owner";
const COLLECTOR: &str = "trustiot_collector";
const COLLECTOR_SECRET: &str = "trustiot-otel-collector-password";

/// What a run can change in ClickHouse, as text, without the volatile columns.
const SNAPSHOT: [&str; 7] = [
    "SELECT database, name, engine, create_table_query FROM system.tables
     WHERE database IN ('trustiot', 'trustiot_logs', 'trustiot_migrate') ORDER BY database, name FORMAT TSV",
    "SELECT name, storage, auth_type, default_roles_list FROM system.users ORDER BY name FORMAT TSV",
    "SELECT name, storage FROM system.roles ORDER BY name FORMAT TSV",
    "SELECT user_name, role_name, granted_role_name, with_admin_option FROM system.role_grants
     ORDER BY ALL FORMAT TSV",
    "SELECT user_name, role_name, access_type, database, table, column, is_partial_revoke, grant_option
     FROM system.grants ORDER BY ALL FORMAT TSV",
    "SELECT name, short_name, database, table, select_filter, is_restrictive, apply_to_list
     FROM system.row_policies ORDER BY ALL FORMAT TSV",
    "SELECT name, sha256, applied_at FROM trustiot_migrate.applied ORDER BY ALL FORMAT TSV",
];

async fn snapshot(clickhouse: &ClickHouse) -> Vec<String> {
    let mut rows = Vec::new();
    for query in SNAPSHOT {
        let text = clickhouse
            .query(query)
            .await
            .unwrap_or_else(|error| panic!("snapshot query failed: {error}\n{query}"));
        rows.push(text);
    }
    rows
}

fn config(db: &Database, clickhouse: &ClickHouse) -> RunConfig {
    let mut config = db.config(MIGRATOR);
    config.platform.clickhouse_url = Some(clickhouse.url.clone());
    config.clickhouse = Some(clickhouse.settings());
    config.passwords = db.passwords(Some((COLLECTOR, COLLECTOR_SECRET)));
    config
}

fn collector_password(db: &Database) -> String {
    db.secrets
        .value(COLLECTOR_SECRET, COLLECTOR)
        .expect("the collector's Secret holds its password")
}

/// The product without its Postgres services, so this binary's runs touch ClickHouse and the
/// Job's own bookkeeping only.
const CLICKHOUSE_ONLY: Registry = Registry {
    services: &[],
    clickhouse: PRODUCT.clickhouse,
    tenant_role_hooks: &[],
};

/// The product's first ClickHouse file, changed after it was applied.
const CHANGED: Registry = Registry {
    services: &[],
    clickhouse: &[Migration {
        name: "0001_databases.sql",
        sql: "SELECT 1;",
    }],
    tenant_role_hooks: &[],
};

#[tokio::test(flavor = "multi_thread")]
async fn clickhouse_ddl_and_the_collector_account() {
    let Some(postgres) = tls_postgres(MIGRATOR_INIT).await else {
        return;
    };
    let Some(clickhouse) = ClickHouse::start().await else {
        return;
    };
    let db = Database::create(postgres, CREATE, "migrate_clickhouse").await;
    db.secrets.declare(COLLECTOR_SECRET);

    // 1. A fresh ClickHouse gets every file once, and the collector account.
    let report = migrate::run(&config(&db, &clickhouse), &CLICKHOUSE_ONLY)
        .await
        .expect("the first run");
    for migration in PRODUCT.clickhouse {
        let expected = format!("applied clickhouse/{}", migration.name);
        assert!(
            report.changes.contains(&expected),
            "missing {expected}: {:?}",
            report.changes
        );
    }
    for expected in [
        format!("generated the password of {COLLECTOR} in Secret {COLLECTOR_SECRET} (it had none)"),
        format!("created ClickHouse account {COLLECTOR}"),
    ] {
        assert!(report.changes.contains(&expected), "{:?}", report.changes);
    }
    let first = collector_password(&db);
    let tables = clickhouse
        .query("SELECT name FROM system.tables WHERE database = 'trustiot_logs' ORDER BY name FORMAT TSV")
        .await
        .expect("list the log tables");
    assert_eq!(tables, "otel_logs\notel_traces\n");

    // 2. The collector logs in, writes logs, and holds the collector role only.
    clickhouse
        .query_as(
            COLLECTOR,
            &first,
            "INSERT INTO trustiot_logs.otel_logs (Body) VALUES ('migrate test')",
        )
        .await
        .expect("the collector writes a log row");
    assert!(
        clickhouse
            .query_as(
                COLLECTOR,
                &first,
                "SELECT count() FROM trustiot_logs.otel_logs"
            )
            .await
            .is_err(),
        "the collector cannot read logs"
    );
    let roles = clickhouse
        .query("SELECT granted_role_name FROM system.role_grants WHERE user_name = 'trustiot_collector' FORMAT TSV")
        .await
        .expect("read the collector's roles");
    assert_eq!(roles, "trustiot_otel_collector\n");
    // 3. A second run changes nothing.
    let before = snapshot(&clickhouse).await;
    let report = migrate::run(&config(&db, &clickhouse), &CLICKHOUSE_ONLY)
        .await
        .expect("the second run");
    assert!(report.changes.is_empty(), "{:?}", report.changes);
    assert_eq!(
        snapshot(&clickhouse).await,
        before,
        "the second run changed ClickHouse"
    );

    // 4. Rotation: a new password generation generates a new password and sets it; the old
    //    one stops working.
    db.set_generation("2");
    let report = migrate::run(&config(&db, &clickhouse), &CLICKHOUSE_ONLY)
        .await
        .expect("the rotation run");
    assert_eq!(
        report.changes,
        [
            format!(
                "generated the password of {COLLECTOR} in Secret {COLLECTOR_SECRET} (password generation 2)"
            ),
            format!("set the password of ClickHouse account {COLLECTOR}"),
        ]
    );
    let second = collector_password(&db);
    assert_ne!(second, first);
    assert!(
        clickhouse
            .query_as(COLLECTOR, &first, "SELECT 1")
            .await
            .is_err()
    );
    clickhouse
        .query_as(COLLECTOR, &second, "SELECT 1")
        .await
        .expect("the new password logs in");

    // 5. An account that gained a grant stops the run, and keeps the grant: dropping it is an
    //    administrator's decision. Once dropped, the next run creates it with its role only.
    clickhouse
        .query("GRANT SELECT ON trustiot.telemetry TO trustiot_collector")
        .await
        .expect("tamper with the account");
    let error = migrate::run(&config(&db, &clickhouse), &CLICKHOUSE_ONLY)
        .await
        .expect_err("a tampered account must stop the run");
    assert!(
        matches!(&error, Error::ClickHouse(text) if text.contains("other roles, grants or settings")),
        "{error}"
    );
    let grants = clickhouse
        .query(
            "SELECT count() FROM system.grants WHERE user_name = 'trustiot_collector' FORMAT TSV",
        )
        .await
        .expect("read the collector's grants");
    assert_eq!(grants, "1\n", "the run changed nothing");
    clickhouse
        .query("DROP USER trustiot_collector")
        .await
        .expect("the administrator drops the account");
    let report = migrate::run(&config(&db, &clickhouse), &CLICKHOUSE_ONLY)
        .await
        .expect("the run after the drop");
    assert_eq!(
        report.changes,
        [format!("created ClickHouse account {COLLECTOR}")]
    );
    let grants = clickhouse
        .query(
            "SELECT count() FROM system.grants WHERE user_name = 'trustiot_collector' FORMAT TSV",
        )
        .await
        .expect("read the collector's grants");
    assert_eq!(grants, "0\n");

    // 6. The role must stay a default role: an account that logs in without it active cannot
    //    write logs, so it stops the run too.
    clickhouse
        .query("SET DEFAULT ROLE NONE TO trustiot_collector")
        .await
        .expect("turn the default role off");
    let error = migrate::run(&config(&db, &clickhouse), &CLICKHOUSE_ONLY)
        .await
        .expect_err("an account without its default role must stop the run");
    assert!(
        matches!(&error, Error::ClickHouse(text) if text.contains("default role")),
        "{error}"
    );
    clickhouse
        .query("SET DEFAULT ROLE trustiot_otel_collector TO trustiot_collector")
        .await
        .expect("turn the default role back on");
    let report = migrate::run(&config(&db, &clickhouse), &CLICKHOUSE_ONLY)
        .await
        .expect("the run after the repair");
    assert!(report.changes.is_empty(), "{:?}", report.changes);

    // 6b. The account check would be moot if the collector role itself had more than
    //     0006_logs.sql grants it: a grant on the role, or another role granted to it, stops the
    //     run until an administrator revokes it.
    for (break_it, repair, what) in [
        (
            &["GRANT SELECT ON trustiot_logs.* TO trustiot_otel_collector"][..],
            &["REVOKE SELECT ON trustiot_logs.* FROM trustiot_otel_collector"][..],
            "a grant on the collector role",
        ),
        (
            &[
                "CREATE ROLE migrate_test_reader",
                "GRANT SELECT ON trustiot.* TO migrate_test_reader",
                "GRANT migrate_test_reader TO trustiot_otel_collector",
            ][..],
            &[
                "REVOKE migrate_test_reader FROM trustiot_otel_collector",
                "DROP ROLE migrate_test_reader",
            ][..],
            "a role granted to the collector role",
        ),
    ] {
        for statement in break_it {
            clickhouse
                .query(statement)
                .await
                .unwrap_or_else(|error| panic!("{statement}: {error}"));
        }
        let error = migrate::run(&config(&db, &clickhouse), &CLICKHOUSE_ONLY)
            .await
            .expect_err(what);
        assert!(
            matches!(&error, Error::ClickHouse(text) if text.contains("beyond 0006_logs.sql")),
            "{what}: {error}"
        );
        for statement in repair {
            clickhouse
                .query(statement)
                .await
                .unwrap_or_else(|error| panic!("{statement}: {error}"));
        }
        let report = migrate::run(&config(&db, &clickhouse), &CLICKHOUSE_ONLY)
            .await
            .expect("the run after the repair");
        assert!(report.changes.is_empty(), "{what}: {:?}", report.changes);
    }

    // 6c. Settings on the account (here readonly = 0, which would let it change settings the
    //     collector role does not need) stop the run too; the administrator drops the account
    //     and the next run creates it again.
    clickhouse
        .query("ALTER USER trustiot_collector SETTINGS readonly = 0")
        .await
        .expect("give the account a setting");
    let error = migrate::run(&config(&db, &clickhouse), &CLICKHOUSE_ONLY)
        .await
        .expect_err("an account with settings must stop the run");
    assert!(
        matches!(&error, Error::ClickHouse(text) if text.contains("grants or settings")),
        "{error}"
    );
    clickhouse
        .query("DROP USER trustiot_collector")
        .await
        .expect("the administrator drops the account");
    let report = migrate::run(&config(&db, &clickhouse), &CLICKHOUSE_ONLY)
        .await
        .expect("the run after the drop");
    assert_eq!(
        report.changes,
        [format!("created ClickHouse account {COLLECTOR}")]
    );

    // 7. A ClickHouse file that changed after it was applied stops the run.
    let error = migrate::run(&config(&db, &clickhouse), &CHANGED)
        .await
        .expect_err("a changed file must stop the run");
    assert!(matches!(error, Error::Changed { .. }), "{error}");
}
