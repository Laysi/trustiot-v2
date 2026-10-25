//! The ClickHouse part of a run: the product's DDL on a fresh ClickHouse, the collector's
//! account (created with only its role and the password the run generates into its Secret,
//! password rotation, a tampered account or collector role refused), the raw and aggregate data
//! retention, and a second run that changes nothing.
//!
//! Needs podman or docker (a throwaway Postgres for the lock and a throwaway ClickHouse);
//! skipped without one unless `CI` is set.

mod support;

use migrate::Error;
use migrate::clickhouse::{managed_ttl_days, managed_ttl_months};
use migrate::config::RunConfig;
use migrate::registry::{Migration, PRODUCT, Registry};
use migrate::retention::{
    AGGREGATE_1H_RETENTION_MONTHS, AGGREGATE_1M_RETENTION_MONTHS, MIN_RAW_RETENTION_DAYS,
    RAW_RETENTION_DAYS,
};
use protocol::telemetry::BUFFER_MAX_AGE_DAYS;
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
    let telemetry_first = report
        .changes
        .iter()
        .position(|change| change.contains("table trustiot.telemetry to 180 days"));
    let commits_after = report
        .changes
        .iter()
        .position(|change| change.contains("table trustiot.batch_commits to 180 days"));
    assert!(
        telemetry_first < commits_after,
        "a shorter retention changes telemetry first: {:?}",
        report.changes
    );
    #[allow(clippy::print_stdout, reason = "test evidence")]
    {
        println!("first run on a fresh ClickHouse: {:?}", report.changes);
    }
    for migration in PRODUCT.clickhouse {
        let expected = format!("applied clickhouse/{}", migration.name);
        assert!(
            report.changes.contains(&expected),
            "missing {expected}: {:?}",
            report.changes
        );
    }
    // The files create the aggregate tables with the platform retention, so the run leaves them.
    assert!(
        !report
            .changes
            .iter()
            .any(|change| change.contains("trustiot.telemetry_1")),
        "{:?}",
        report.changes
    );
    for expected in [
        format!("generated the password of {COLLECTOR} in Secret {COLLECTOR_SECRET} (it had none)"),
        format!("created ClickHouse account {COLLECTOR}"),
        // The files create both tables with 400 days, as an existing deployment has them; the
        // same run sets the platform retention (Laysi/trustiot-v2#92), completion records last.
        "set the retention of ClickHouse table trustiot.telemetry to 180 days (was 400)".to_owned(),
        "set the retention of ClickHouse table trustiot.batch_commits to 180 days (was 400)"
            .to_owned(),
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

    // 6d. Leftover accounts (Laysi/trustiot-v2#60 round 4, R3): with the collector off, its
    //     account can no longer log in, though it stays (HOST NONE, never dropped); turned on
    //     again, it logs in with its password. A renamed account retires the old one.
    let password = collector_password(&db);
    let mut off = config(&db, &clickhouse);
    off.passwords = db.passwords(None);
    let report = migrate::run(&off, &CLICKHOUSE_ONLY)
        .await
        .expect("the run with the collector off");
    assert_eq!(
        report.changes,
        [format!(
            "revoked the login of ClickHouse account {COLLECTOR}: the collector no longer uses it"
        )]
    );
    assert!(
        clickhouse
            .query_as(COLLECTOR, &password, "SELECT 1")
            .await
            .is_err(),
        "a retired account cannot log in"
    );
    let report = migrate::run(&off, &CLICKHOUSE_ONLY)
        .await
        .expect("the second run with the collector off");
    assert!(report.changes.is_empty(), "{:?}", report.changes);
    let report = migrate::run(&config(&db, &clickhouse), &CLICKHOUSE_ONLY)
        .await
        .expect("the run with the collector on again");
    assert_eq!(
        report.changes,
        [
            format!("set the password of ClickHouse account {COLLECTOR}"),
            format!("let ClickHouse account {COLLECTOR} log in again")
        ]
    );
    clickhouse
        .query_as(COLLECTOR, &password, "SELECT 1")
        .await
        .expect("the account logs in again with its password");
    let mut renamed = config(&db, &clickhouse);
    renamed.passwords = db.passwords(Some(("trustiot_collector_b", COLLECTOR_SECRET)));
    let report = migrate::run(&renamed, &CLICKHOUSE_ONLY)
        .await
        .expect("the run with a renamed account");
    assert!(
        report.changes.contains(&format!(
            "revoked the login of ClickHouse account {COLLECTOR}: the collector no longer uses it"
        )),
        "{:?}",
        report.changes
    );
    assert!(
        report
            .changes
            .contains(&"created ClickHouse account trustiot_collector_b".to_owned())
    );
    clickhouse
        .query("DROP USER trustiot_collector_b")
        .await
        .expect("clean up the renamed account");
    let report = migrate::run(&config(&db, &clickhouse), &CLICKHOUSE_ONLY)
        .await
        .expect("back to the first account");
    assert_eq!(
        report.changes,
        [
            format!("set the password of ClickHouse account {COLLECTOR}"),
            format!("let ClickHouse account {COLLECTOR} log in again")
        ]
    );

    // 7. The raw data retention (Laysi/trustiot-v2#92), then the aggregate retention
    //    (Laysi/trustiot-v2#141).
    raw_retention(&db, &clickhouse).await;
    aggregate_retention(&db, &clickhouse).await;

    // 8. A ClickHouse file that changed after it was applied stops the run.
    let error = migrate::run(&config(&db, &clickhouse), &CHANGED)
        .await
        .expect_err("a changed file must stop the run");
    assert!(matches!(error, Error::Changed { .. }), "{error}");
}

/// Both tables' engine settings, from which the TTL is read.
const TABLE_ENGINES: &str = "SELECT name, engine_full FROM system.tables
WHERE database = 'trustiot' AND name IN ('telemetry', 'batch_commits') FORMAT JSONEachRow";

/// The retention in days of the telemetry and completion tables, as the server reports it.
async fn ttl_days(clickhouse: &ClickHouse) -> (u32, u32) {
    let rows = clickhouse
        .query(TABLE_ENGINES)
        .await
        .unwrap_or_else(|error| panic!("read the TTLs: {error}"));
    let mut telemetry = None;
    let mut commits = None;
    for line in rows.lines().filter(|line| !line.is_empty()) {
        let row: serde_json::Value =
            serde_json::from_str(line).unwrap_or_else(|error| panic!("{error}: {line}"));
        let engine_full = row["engine_full"].as_str().expect("a string column");
        match row["name"].as_str() {
            Some("telemetry") => telemetry = Some(managed_ttl_days(engine_full, "received_at")),
            Some("batch_commits") => commits = Some(managed_ttl_days(engine_full, "committed_at")),
            other => panic!("unexpected table {other:?}"),
        }
    }
    let read = |ttl: Option<Result<u32, String>>, table: &str| {
        ttl.unwrap_or_else(|| panic!("no table {table}"))
            .unwrap_or_else(|problem| panic!("{table}: {problem}"))
    };
    (read(telemetry, "telemetry"), read(commits, "batch_commits"))
}

fn with_retention(db: &Database, clickhouse: &ClickHouse, days: Option<u32>) -> RunConfig {
    let mut config = config(db, clickhouse);
    config
        .clickhouse
        .as_mut()
        .expect("ClickHouse settings")
        .raw_retention_days = days;
    config
}

/// An account with the ingest role, which reads every row (the bootstrap admin, which writes the
/// rows, reads none: deny_by_default applies to every account no other policy names). It logs in
/// without a password: the container is local and thrown away after the test.
const INGEST_ACCOUNT: &str = "migrate_test_ingest";
const CREATE_INGEST_ACCOUNT: &str =
    "CREATE USER migrate_test_ingest IDENTIFIED WITH no_password DEFAULT ROLE trustiot_ingest";
const DROP_INGEST_ACCOUNT: &str = "DROP USER migrate_test_ingest";

/// Three readings of one tenant: with a timestamp three years old (a gateway whose clock was never
/// set; a clock can be off by any amount) that arrived now, one received a day before the
/// retention ends, and one received a day after it ended, however recent its timestamp. The
/// ingest service stamps `received_at` itself; the test writes it, to have rows that arrived
/// long ago.
const INSERT_READINGS: &str = "INSERT INTO trustiot.telemetry (tenant_id, gateway_id, stream_id,
    batch_seq, device_id, point, ts, received_at, value_type, value_float)
SELECT {tenant:UUID}, generateUUIDv4(), generateUUIDv4(), 1, generateUUIDv4(), point, ts,
    received_at, 'float', 1.5
FROM VALUES('point String, ts DateTime64(3, \\'UTC\\'), received_at DateTime64(3, \\'UTC\\')',
    ('old_timestamp_received_now', now64(3) - toIntervalDay(1095), now64(3)),
    ('received_within_retention', now64(3), now64(3) - toIntervalDay({days:UInt32} - 1)),
    ('received_past_retention', now64(3), now64(3) - toIntervalDay({days:UInt32} + 1)))";

/// Completion records of the same tenant: committed now (seq 1), a day before the retention ends
/// (seq 2) and a day after it ended (seq 3).
const INSERT_COMMITS: &str = "INSERT INTO trustiot.batch_commits (tenant_id, gateway_id,
    stream_id, seq, row_count, committed_at)
SELECT {tenant:UUID}, generateUUIDv4(), generateUUIDv4(), seq, 1, committed_at
FROM VALUES('seq UInt64, committed_at DateTime64(3, \\'UTC\\')',
    (1, now64(3)),
    (2, now64(3) - toIntervalDay({days:UInt32} - 1)),
    (3, now64(3) - toIntervalDay({days:UInt32} + 1)))";

/// OPTIMIZE ... FINAL merges every part of the table, and a merge applies its TTL.
const MERGE_TELEMETRY: &str = "OPTIMIZE TABLE trustiot.telemetry FINAL";
const MERGE_COMMITS: &str = "OPTIMIZE TABLE trustiot.batch_commits FINAL";
const KEPT_READINGS: &str = "SELECT point FROM trustiot.telemetry WHERE tenant_id = {tenant:UUID} ORDER BY point FORMAT TSV";
const KEPT_COMMITS: &str = "SELECT seq FROM trustiot.batch_commits WHERE tenant_id = {tenant:UUID} ORDER BY seq FORMAT TSV";

/// The run sets the platform retention as the TTL of the telemetry and completion tables; rows
/// expire by the server's receive time, so readings with an old gateway timestamp that just
/// arrived stay; a changed setting changes both TTLs in an order that never lets completion
/// records expire before their rows; a setting below the minimum (33 days) stops the run before
/// it changes anything; a TTL the run does not recognise stops it too.
async fn raw_retention(db: &Database, clickhouse: &ClickHouse) {
    let platform = RAW_RETENTION_DAYS;
    assert_eq!(
        platform, 180,
        "the value docs/architecture/data-retention.md names"
    );

    // a. The first run (step 1) set the platform value on both tables; another run changes
    //    nothing.
    assert_eq!(ttl_days(clickhouse).await, (platform, platform));
    let report = migrate::run(&with_retention(db, clickhouse, None), &CLICKHOUSE_ONLY)
        .await
        .expect("a run with the platform retention");
    assert!(report.changes.is_empty(), "{:?}", report.changes);

    // b. Rows expire by the server's receive time, not the gateway's timestamp
    //    (Laysi/trustiot#180).
    clickhouse
        .query(CREATE_INGEST_ACCOUNT)
        .await
        .expect("create the ingest account");
    // Any tenant but the all-zero one: the throwaway ClickHouse holds no other rows.
    let days = platform.to_string();
    let params = [
        ("tenant", "00000000-0000-4000-8000-000000000092"),
        ("days", days.as_str()),
    ];
    for insert in [INSERT_READINGS, INSERT_COMMITS] {
        clickhouse
            .query_with(insert, &params)
            .await
            .unwrap_or_else(|error| panic!("{insert}: {error}"));
    }
    for merge in [MERGE_TELEMETRY, MERGE_COMMITS] {
        clickhouse
            .query(merge)
            .await
            .unwrap_or_else(|error| panic!("{merge}: {error}"));
    }
    let kept = clickhouse
        .query_as_with(INGEST_ACCOUNT, "", KEPT_READINGS, &params[..1])
        .await
        .expect("read the readings");
    assert_eq!(
        kept, "old_timestamp_received_now\nreceived_within_retention\n",
        "telemetry keeps the rows received within the retention, whatever their timestamp"
    );
    let commits = clickhouse
        .query_as_with(INGEST_ACCOUNT, "", KEPT_COMMITS, &params[..1])
        .await
        .expect("read the completion records");
    assert_eq!(
        commits, "1\n2\n",
        "completion records past the retention expire"
    );
    #[allow(clippy::print_stdout, reason = "test evidence")]
    {
        println!("retention {platform} days; telemetry rows kept after the TTL merge:\n{kept}");
        println!("completion records kept after the TTL merge (seq):\n{commits}");
    }
    clickhouse
        .query(DROP_INGEST_ACCOUNT)
        .await
        .expect("drop the ingest account");

    // c. A longer retention: the completion records first, so they never expire before their
    //    rows; a shorter one: the telemetry first. A repeated run changes nothing.
    let mut evidence = Vec::new();
    let report = migrate::run(&with_retention(db, clickhouse, Some(365)), &CLICKHOUSE_ONLY)
        .await
        .expect("a run with a longer retention");
    evidence.push(format!("365 days: {:?}", report.changes));
    assert_eq!(
        report.changes,
        [
            "set the retention of ClickHouse table trustiot.batch_commits to 365 days (was 180)",
            "set the retention of ClickHouse table trustiot.telemetry to 365 days (was 180)",
        ]
    );
    assert_eq!(ttl_days(clickhouse).await, (365, 365));
    let report = migrate::run(&with_retention(db, clickhouse, None), &CLICKHOUSE_ONLY)
        .await
        .expect("back to the platform retention");
    evidence.push(format!("back to {platform} days: {:?}", report.changes));
    assert_eq!(
        report.changes,
        [
            "set the retention of ClickHouse table trustiot.telemetry to 180 days (was 365)",
            "set the retention of ClickHouse table trustiot.batch_commits to 180 days (was 365)",
        ]
    );
    let report = migrate::run(&with_retention(db, clickhouse, None), &CLICKHOUSE_ONLY)
        .await
        .expect("a repeated run");
    assert!(report.changes.is_empty(), "{:?}", report.changes);

    // d. A retention below the minimum (the gateway buffer's maximum age, the reconcile window
    //    and two days) stops the run before it touches Postgres, a Secret or ClickHouse.
    let before = snapshot(clickhouse).await;
    for days in [MIN_RAW_RETENTION_DAYS - 1, BUFFER_MAX_AGE_DAYS, 7] {
        let error = migrate::run(
            &with_retention(db, clickhouse, Some(days)),
            &CLICKHOUSE_ONLY,
        )
        .await
        .expect_err("a retention below the minimum must be refused");
        let text = error.to_string();
        assert!(
            text.contains("clickhouse.raw_retention_days")
                && text.contains(&format!(
                    "gateway buffer's maximum age of {BUFFER_MAX_AGE_DAYS} days"
                )),
            "{days}: {text}"
        );
        evidence.push(format!("{days} days refused: {text}"));
    }
    assert_eq!(
        snapshot(clickhouse).await,
        before,
        "a refused run changed ClickHouse"
    );

    // e. A TTL someone changed by hand (here to the gateway's timestamp) is not overwritten: the
    //    run stops and names it; set back, the run goes on.
    clickhouse
        .query(TAMPER_TTL)
        .await
        .expect("change the TTL by hand");
    let error = migrate::run(&with_retention(db, clickhouse, None), &CLICKHOUSE_ONLY)
        .await
        .expect_err("an unrecognised TTL must stop the run");
    assert!(
        matches!(&error, Error::ClickHouse(text)
            if text.contains("trustiot.telemetry") && text.contains("not one the migration Job manages")),
        "{error}"
    );
    evidence.push(format!("TTL changed by hand: {error}"));
    clickhouse
        .query(REPAIR_TTL)
        .await
        .expect("set the TTL back");
    let report = migrate::run(&with_retention(db, clickhouse, None), &CLICKHOUSE_ONLY)
        .await
        .expect("the run after the repair");
    assert_eq!(
        report.changes,
        ["set the retention of ClickHouse table trustiot.telemetry to 180 days (was 200)"]
    );
    evidence.push(format!("after the repair: {:?}", report.changes));
    #[allow(clippy::print_stdout, reason = "test evidence")]
    {
        println!("retention changes, step by step:\n{}", evidence.join("\n"));
    }
}

/// The aggregate tables' engine settings, from which their TTL is read.
const AGGREGATE_ENGINES: &str = "SELECT name, engine_full FROM system.tables
WHERE database = 'trustiot' AND name IN ('telemetry_1m', 'telemetry_1h') FORMAT JSONEachRow";

/// The retention in months of the one-minute and one-hour tables, as the server reports it.
async fn ttl_months(clickhouse: &ClickHouse) -> (u32, u32) {
    let rows = clickhouse
        .query(AGGREGATE_ENGINES)
        .await
        .unwrap_or_else(|error| panic!("read the aggregate TTLs: {error}"));
    let mut minute = None;
    let mut hour = None;
    for line in rows.lines().filter(|line| !line.is_empty()) {
        let row: serde_json::Value =
            serde_json::from_str(line).unwrap_or_else(|error| panic!("{error}: {line}"));
        let engine_full = row["engine_full"].as_str().expect("a string column");
        let months = managed_ttl_months(engine_full, "received_day");
        match row["name"].as_str() {
            Some("telemetry_1m") => minute = Some(months),
            Some("telemetry_1h") => hour = Some(months),
            other => panic!("unexpected table {other:?}"),
        }
    }
    let read = |ttl: Option<Result<u32, String>>, table: &str| {
        ttl.unwrap_or_else(|| panic!("no table {table}"))
            .unwrap_or_else(|problem| panic!("{table}: {problem}"))
    };
    (read(minute, "telemetry_1m"), read(hour, "telemetry_1h"))
}

/// The raw retention in days and the one-minute and one-hour retention in months.
fn with_retentions(
    db: &Database,
    clickhouse: &ClickHouse,
    raw_days: Option<u32>,
    minute_months: Option<u32>,
    hour_months: Option<u32>,
) -> RunConfig {
    let mut config = with_retention(db, clickhouse, raw_days);
    let settings = config.clickhouse.as_mut().expect("ClickHouse settings");
    settings.aggregate_1m_retention_months = minute_months;
    settings.aggregate_1h_retention_months = hour_months;
    config
}

/// The run sets the aggregate retention as the TTL of the one-minute and one-hour tables, in
/// months; with the raw retention it changes the four tables in an order that never lets a table
/// expire its rows before the next shorter-lived one (one-hour, one-minute, completion records,
/// raw rows); refused settings stop the run before it changes anything; an aggregate TTL the run
/// does not recognise stops it too.
async fn aggregate_retention(db: &Database, clickhouse: &ClickHouse) {
    assert_eq!(
        (AGGREGATE_1M_RETENTION_MONTHS, AGGREGATE_1H_RETENTION_MONTHS),
        (13, 48),
        "the values docs/architecture/data-retention.md names"
    );
    let mut evidence = Vec::new();

    // a. The files created both tables with the platform values, as the server reports them.
    assert_eq!(ttl_months(clickhouse).await, (13, 48));
    assert_eq!(ttl_days(clickhouse).await, (180, 180));

    // b. Longer everywhere: the one-hour table first, the raw rows last.
    let report = migrate::run(
        &with_retentions(db, clickhouse, Some(400), Some(14), Some(60)),
        &CLICKHOUSE_ONLY,
    )
    .await
    .expect("a run with longer retentions");
    evidence.push(format!("400 days, 14 and 60 months: {:?}", report.changes));
    assert_eq!(
        report.changes,
        [
            "set the retention of ClickHouse table trustiot.telemetry_1h to 60 months (was 48)",
            "set the retention of ClickHouse table trustiot.telemetry_1m to 14 months (was 13)",
            "set the retention of ClickHouse table trustiot.batch_commits to 400 days (was 180)",
            "set the retention of ClickHouse table trustiot.telemetry to 400 days (was 180)",
        ]
    );
    // Read back in the form the run's own MODIFY TTL stores.
    assert_eq!(ttl_months(clickhouse).await, (14, 60));
    assert_eq!(ttl_days(clickhouse).await, (400, 400));

    // c. Back to the platform values: the raw rows first, the one-hour table last. A repeated
    //    run changes nothing.
    let report = migrate::run(&with_retention(db, clickhouse, None), &CLICKHOUSE_ONLY)
        .await
        .expect("back to the platform retention");
    evidence.push(format!("back to the platform values: {:?}", report.changes));
    assert_eq!(
        report.changes,
        [
            "set the retention of ClickHouse table trustiot.telemetry to 180 days (was 400)",
            "set the retention of ClickHouse table trustiot.batch_commits to 180 days (was 400)",
            "set the retention of ClickHouse table trustiot.telemetry_1m to 13 months (was 14)",
            "set the retention of ClickHouse table trustiot.telemetry_1h to 48 months (was 60)",
        ]
    );
    assert_eq!(ttl_months(clickhouse).await, (13, 48));
    let report = migrate::run(&with_retention(db, clickhouse, None), &CLICKHOUSE_ONLY)
        .await
        .expect("a repeated run");
    assert!(report.changes.is_empty(), "{:?}", report.changes);

    // d. A refused setting stops the run before it touches Postgres, a Secret or ClickHouse,
    //    naming the setting and its rule.
    let before = snapshot(clickhouse).await;
    for (raw, minute, hour, key, reason) in [
        (
            None,
            Some(12),
            None,
            "clickhouse.aggregate_1m_retention_months",
            "year-on-year",
        ),
        (
            None,
            None,
            Some(12),
            "clickhouse.aggregate_1h_retention_months",
            "at least as long as the finer table",
        ),
        (
            None,
            Some(121),
            Some(121),
            "clickhouse.aggregate_1m_retention_months",
            "another unit",
        ),
        (
            Some(400),
            None,
            None,
            "clickhouse.aggregate_1m_retention_months",
            "shorter than the raw data retention of 400 days",
        ),
    ] {
        let error = migrate::run(
            &with_retentions(db, clickhouse, raw, minute, hour),
            &CLICKHOUSE_ONLY,
        )
        .await
        .expect_err("a refused aggregate retention");
        let text = error.to_string();
        assert!(
            text.contains(key) && text.contains(reason),
            "{raw:?} {minute:?} {hour:?}: {text}"
        );
        evidence.push(format!(
            "{raw:?} days, {minute:?} and {hour:?} months refused: {text}"
        ));
    }
    assert_eq!(
        snapshot(clickhouse).await,
        before,
        "a refused run changed ClickHouse"
    );

    // e. An aggregate TTL someone changed by hand (here to the bucket, the gateway's time) is not
    //    overwritten: the run stops before it changes any table and names the form to set back;
    //    set back, the run goes on.
    clickhouse
        .query(TAMPER_AGGREGATE_TTL)
        .await
        .expect("change the aggregate TTL by hand");
    let before = snapshot(clickhouse).await;
    let error = migrate::run(
        &with_retentions(db, clickhouse, Some(365), None, Some(60)),
        &CLICKHOUSE_ONLY,
    )
    .await
    .expect_err("an unrecognised aggregate TTL must stop the run");
    assert!(
        matches!(&error, Error::ClickHouse(text)
            if text.contains("trustiot.telemetry_1m")
                && text.contains("not one the migration Job manages")
                && text.contains("TTL received_day + toIntervalMonth(<months>) + toIntervalDay(1)")),
        "{error}"
    );
    assert_eq!(
        snapshot(clickhouse).await,
        before,
        "the stopped run changed another table"
    );
    evidence.push(format!("aggregate TTL changed by hand: {error}"));
    clickhouse
        .query(REPAIR_AGGREGATE_TTL)
        .await
        .expect("set the aggregate TTL back");
    let report = migrate::run(&with_retention(db, clickhouse, None), &CLICKHOUSE_ONLY)
        .await
        .expect("the run after the repair");
    assert_eq!(
        report.changes,
        ["set the retention of ClickHouse table trustiot.telemetry_1m to 13 months (was 14)"]
    );
    evidence.push(format!("after the repair: {:?}", report.changes));
    #[allow(clippy::print_stdout, reason = "test evidence")]
    {
        println!(
            "aggregate retention changes, step by step:\n{}",
            evidence.join("\n")
        );
    }
}

const TAMPER_AGGREGATE_TTL: &str =
    "ALTER TABLE trustiot.telemetry_1m MODIFY TTL bucket + toIntervalMonth(13)";
const REPAIR_AGGREGATE_TTL: &str = "ALTER TABLE trustiot.telemetry_1m
MODIFY TTL received_day + toIntervalMonth(14) + toIntervalDay(1)";

const TAMPER_TTL: &str =
    "ALTER TABLE trustiot.telemetry MODIFY TTL toDateTime(ts) + toIntervalDay(180)";
const REPAIR_TTL: &str =
    "ALTER TABLE trustiot.telemetry MODIFY TTL toDateTime(received_at) + toIntervalDay(200)";
