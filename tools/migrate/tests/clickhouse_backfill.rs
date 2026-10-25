//! The run's backfill of the one-minute and one-hour aggregates (Laysi/trustiot-v2#141) on a
//! ClickHouse that held raw rows before 0010_telemetry_rollups.sql:
//!
//! 1. On a deployment whose raw retention is 401 days, the first run applies every file, sets
//!    the retention (one-minute aggregates 14 months) and then backfills every receive day the
//!    raw retention allows, less its three oldest days: the newest day that the one-minute
//!    table's 13 months drop gets its cells (with 401 days that day is always inside the
//!    backfill), and they survive a merge although the files create the one-minute table with
//!    13 months; an older day gets none. The backfill views show their rows to nobody after the
//!    run, and a second run changes nothing.
//! 2. A run with a 200-day raw retention lets the raw rows of the older days expire (one day
//!    only in part), and the cells of yesterday are deleted. Then `trustiot_migrate.applied` is
//!    lost and a run under a second administrator account applies every file again and
//!    backfills again: it restores yesterday's cells, leaves the cells of every day whose raw
//!    rows may have expired as they were (the partly expired day included), and leaves the
//!    backfill views showing nothing again.
//!
//! Its own binary, because the tests of one binary share one Postgres and one ClickHouse is
//! enough at a time. Needs podman or docker; skipped without one unless `CI` is set.

#![allow(
    clippy::print_stdout,
    reason = "the printed steps are the test evidence"
)]

mod support;

use std::time::{Duration, Instant};

use migrate::clickhouse::BACKFILL_RECORD;
use migrate::config::RunConfig;
use migrate::registry::{PRODUCT, Registry};
use support::postgres::tls_postgres;
use support::{ClickHouse, Database, MIGRATOR, MIGRATOR_INIT, random_hex};

const CREATE: &str = "CREATE DATABASE migrate_clickhouse_backfill OWNER trustiot_owner";

/// The product without its Postgres services, so this binary's runs touch ClickHouse and the
/// Job's own bookkeeping only.
const CLICKHOUSE_ONLY: Registry = Registry {
    services: &[],
    clickhouse: PRODUCT.clickhouse,
    tenant_role_hooks: &[],
};

const TENANT: &str = "00000000-0000-4000-8000-000000014110";
const GATEWAY: &str = "00000000-0000-4000-8000-000000014111";
const DEVICE: &str = "00000000-0000-4000-8000-000000014112";
const STREAM: &str = "00000000-0000-4000-8000-000000014113";
/// Readings per batch, 20 seconds apart, ending at the receive time.
const ROWS: &str = "4";

/// The receive days of the test, by the server's clock: today; the newest day the one-minute
/// table's 13 months (and a day) drop; the oldest day a 401-day raw retention backfills and the
/// day before it; a month ago; yesterday; the day a 200-day raw retention expires in part, and
/// the oldest day it backfills.
const DAYS: &str = "SELECT toString(today), toString(today - 1 - toIntervalMonth(13)),
  toString(today - 398), toString(today - 399), toString(today - 30), toString(today - 1),
  toString(today - 200), toString(today - 197)
FROM (SELECT toDate(now('UTC')) AS today) FORMAT TSV";
/// Seconds until the next midnight (UTC) on the server.
const TO_MIDNIGHT: &str =
    "SELECT dateDiff('second', now('UTC'), toStartOfDay(now('UTC')) + toIntervalDay(1)) FORMAT TSV";
/// How close to midnight (UTC) the test may start: it names days by their distance from today.
const MIDNIGHT_MARGIN_SECONDS: u64 = 15 * 60;

/// The raw and completion TTLs as the Job of a release before Laysi/trustiot-v2#141 set them for
/// a 401-day raw retention: the files create the tables with 400 days, and the run backfills
/// only the days the TTL before it kept.
const EARLIER_RETENTION: [&str; 2] = [
    "ALTER TABLE trustiot.batch_commits
MODIFY TTL toDateTime(committed_at) + toIntervalDay(401)",
    "ALTER TABLE trustiot.telemetry
MODIFY TTL toDateTime(received_at) + toIntervalDay(401)",
];

/// A batch as the ingest service writes it, as the admin (who may insert but reads no rows).
const INSERT_BATCH: &str = "INSERT INTO trustiot.telemetry (tenant_id, gateway_id, stream_id,
    batch_seq, device_id, point, ts, received_at, value_type, value_float)
SELECT {tenant:UUID}, {gateway:UUID}, {stream:UUID}, {seq:UInt64}, {device:UUID}, 'energy',
    {received_at:DateTime64(3, 'UTC')} - toIntervalSecond(20 * number),
    {received_at:DateTime64(3, 'UTC')}, 'float', 1.5 * number
FROM numbers({rows:UInt32})";

/// A reader of every row of the raw and the aggregate tables, for the comparisons. Test-only:
/// the product has no such account.
const CREATE_READER: &str =
    "CREATE USER migrate_backfill_reader IDENTIFIED WITH sha256_password BY {password:String}";
const READER: &str = "migrate_backfill_reader";
const READER_ACCESS: [&str; 4] = [
    "GRANT SELECT ON trustiot.telemetry TO migrate_backfill_reader",
    "GRANT SELECT ON trustiot.telemetry_1m TO migrate_backfill_reader",
    "GRANT SELECT ON trustiot.telemetry_1h TO migrate_backfill_reader",
    "CREATE ROW POLICY migrate_backfill_all_rows
ON trustiot.telemetry, trustiot.telemetry_1m, trustiot.telemetry_1h
AS PERMISSIVE FOR SELECT USING 1 TO migrate_backfill_reader",
];

/// A second administrator account, as a deployment whose ClickHouse administrator changed.
const CREATE_SECOND_ADMIN: &str =
    "CREATE USER migrate_backfill_admin IDENTIFIED WITH sha256_password BY {password:String}";
const SECOND_ADMIN: &str = "migrate_backfill_admin";
const GRANT_SECOND_ADMIN: &str =
    "GRANT CURRENT GRANTS ON *.* TO migrate_backfill_admin WITH GRANT OPTION";

/// The tenant's raw rows, and its cells in each aggregate table, per receive day: readings,
/// numeric readings, sum, minimum, maximum. Days whose cells sum to no reading are left out.
const RAW_DAYS: &str = "SELECT 'raw', toString(toDate(received_at)), count(), count(),
  round(sum(value_float), 6), min(value_float), max(value_float)
FROM trustiot.telemetry WHERE tenant_id = {tenant:UUID}
GROUP BY 2 ORDER BY 2 FORMAT TSV";
macro_rules! cell_days {
    ($table:literal) => {
        concat!(
            "SELECT '",
            $table,
            "', toString(received_day), sum(readings), sum(numeric_readings),
  round(sum(value_sum), 6), min(value_min), max(value_max)
FROM trustiot.",
            $table,
            " WHERE tenant_id = {tenant:UUID}
GROUP BY received_day HAVING sum(readings) != 0 ORDER BY received_day FORMAT TSV"
        )
    };
}
const CELL_DAYS: [&str; 2] = [cell_days!("telemetry_1m"), cell_days!("telemetry_1h")];

const OPTIMIZE: [&str; 3] = [
    "OPTIMIZE TABLE trustiot.telemetry FINAL",
    "OPTIMIZE TABLE trustiot.telemetry_1m FINAL",
    "OPTIMIZE TABLE trustiot.telemetry_1h FINAL",
];
const PENDING_MUTATIONS: &str =
    "SELECT count() FROM system.mutations WHERE database = 'trustiot' AND NOT is_done FORMAT TSV";
/// How long the test waits for the TTL mutations a run starts.
const MUTATIONS_WAIT: Duration = Duration::from_secs(300);

/// The cells of one receive day go, as a lost part or a mistaken deletion would take them.
const DELETE_CELLS: [&str; 2] = [
    "ALTER TABLE trustiot.telemetry_1m DELETE WHERE received_day = {day:Date}
SETTINGS mutations_sync = 2",
    "ALTER TABLE trustiot.telemetry_1h DELETE WHERE received_day = {day:Date}
SETTINGS mutations_sync = 2",
];

const POLICIES: &str = "SELECT table, apply_to_all, apply_to_list FROM system.row_policies
WHERE short_name = 'migration_account' ORDER BY table FORMAT TSV";
const POLICIES_UNBOUND: &str = "telemetry_1h_backfill\t0\t[]\ntelemetry_1m_backfill\t0\t[]\n";
/// What a Job stopped between showing and hiding the backfill views, or an administrator's hand,
/// leaves behind: one policy names the admin, the other everyone.
const LEFT_BOUND: [&str; 2] = [
    "ALTER ROW POLICY migration_account ON trustiot.telemetry_1m_backfill TO CURRENT_USER",
    "ALTER ROW POLICY migration_account ON trustiot.telemetry_1h_backfill TO ALL",
];
/// The runbook's read of the policies (docs/runbooks/migrations.md, `policies`): whether each
/// still names the account that checks.
const RUNBOOK_POLICIES: &str = "SELECT table, apply_to_all, apply_to_list,
    has(apply_to_list, currentUser()) AS mine FROM system.row_policies
WHERE database = 'trustiot' AND short_name = 'migration_account' ORDER BY table FORMAT TSV";
/// The runbook's check of one receive day (docs/runbooks/migrations.md).
const CHECK_DAY: &str = "SELECT count() FROM trustiot.telemetry_1m_backfill(first_day = {day:Date},
    last_day = {day:Date}, raw_retention_days = {raw:UInt32}) FORMAT TSV";
const SHOW_BACKFILL_VIEWS: &str = "ALTER ROW POLICY migration_account
ON trustiot.telemetry_1m_backfill, trustiot.telemetry_1h_backfill TO CURRENT_USER";
const HIDE_BACKFILL_VIEWS: &str = "ALTER ROW POLICY migration_account
ON trustiot.telemetry_1m_backfill, trustiot.telemetry_1h_backfill TO NONE";
const LOSE_RECORDS: &str = "TRUNCATE TABLE trustiot_migrate.applied";
const RECORDS: &str =
    "SELECT name FROM trustiot_migrate.applied WHERE name = {name:String} FORMAT TSV";

fn config(db: &Database, clickhouse: &ClickHouse, raw_days: u32) -> RunConfig {
    let mut config = db.config(MIGRATOR);
    config.platform.clickhouse_url = Some(clickhouse.url.clone());
    let mut settings = clickhouse.settings();
    settings.raw_retention_days = Some(raw_days);
    settings.aggregate_1m_retention_months = Some(14);
    config.clickhouse = Some(settings);
    config
}

/// The receive days of [`DAYS`].
struct Days {
    today: String,
    thirteen_months: String,
    oldest_401: String,
    before_401: String,
    month: String,
    yesterday: String,
    cut_off_200: String,
    oldest_200: String,
}

fn rows(text: &str) -> Vec<Vec<String>> {
    text.lines()
        .map(|line| line.split('\t').map(str::to_owned).collect())
        .collect()
}

async fn admin(clickhouse: &ClickHouse, sql: &str, params: &[(&str, &str)]) -> String {
    clickhouse
        .query_with(sql, params)
        .await
        .unwrap_or_else(|error| panic!("{sql}\n{error}"))
}

async fn insert(clickhouse: &ClickHouse, seq: u32, received_at: &str) {
    let seq = seq.to_string();
    admin(
        clickhouse,
        INSERT_BATCH,
        &[
            ("tenant", TENANT),
            ("gateway", GATEWAY),
            ("stream", STREAM),
            ("seq", seq.as_str()),
            ("device", DEVICE),
            ("received_at", received_at),
            ("rows", ROWS),
        ],
    )
    .await;
}

/// The raw rows and both tables' cells per receive day, as the reader sees them.
async fn per_day(clickhouse: &ClickHouse, reader: &str) -> Vec<Vec<String>> {
    let mut all = Vec::new();
    for query in std::iter::once(RAW_DAYS).chain(CELL_DAYS) {
        let text = clickhouse
            .query_as_with(READER, reader, query, &[("tenant", TENANT)])
            .await
            .unwrap_or_else(|error| panic!("{query}\n{error}"));
        all.extend(rows(&text));
    }
    all
}

/// The rows of `all` for `day`, as (source, readings).
fn on_day(all: &[Vec<String>], day: &str) -> Vec<(String, String)> {
    all.iter()
        .filter(|row| row[1] == day)
        .map(|row| (row[0].clone(), row[2].clone()))
        .collect()
}

/// Each table's row for `day`, the raw row's figures included; panics unless all three agree.
fn assert_cells_equal_raw(all: &[Vec<String>], day: &str, step: &str) {
    let found: Vec<&Vec<String>> = all.iter().filter(|row| row[1] == day).collect();
    assert_eq!(found.len(), 3, "{step}: {day}: {found:?}");
    for row in &found[1..] {
        assert_eq!(row[2..], found[0][2..], "{step}: {day}: {found:?}");
    }
}

async fn wait_for_mutations(clickhouse: &ClickHouse) {
    let started = Instant::now();
    loop {
        if admin(clickhouse, PENDING_MUTATIONS, &[]).await.trim() == "0" {
            return;
        }
        assert!(
            started.elapsed() < MUTATIONS_WAIT,
            "the TTL mutations of the trustiot tables did not finish within {MUTATIONS_WAIT:?}"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

async fn merge(clickhouse: &ClickHouse) {
    wait_for_mutations(clickhouse).await;
    for statement in OPTIMIZE {
        admin(clickhouse, statement, &[]).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_run_backfills_the_aggregates_within_the_raw_retention() {
    let Some(postgres) = tls_postgres(MIGRATOR_INIT).await else {
        return;
    };
    let Some(clickhouse) = ClickHouse::start().await else {
        return;
    };
    let db = Database::create(postgres, CREATE, "migrate_clickhouse_backfill").await;

    let to_midnight: u64 = admin(&clickhouse, TO_MIDNIGHT, &[])
        .await
        .trim()
        .parse()
        .expect("seconds");
    if to_midnight < MIDNIGHT_MARGIN_SECONDS {
        println!("waiting {to_midnight} s for the UTC date to change");
        tokio::time::sleep(Duration::from_secs(to_midnight + 5)).await;
    }
    let fields = rows(&admin(&clickhouse, DAYS, &[]).await)
        .pop()
        .expect("the days");
    let days = Days {
        today: fields[0].clone(),
        thirteen_months: fields[1].clone(),
        oldest_401: fields[2].clone(),
        before_401: fields[3].clone(),
        month: fields[4].clone(),
        yesterday: fields[5].clone(),
        cut_off_200: fields[6].clone(),
        oldest_200: fields[7].clone(),
    };
    // Today - 1 - 13 months is at most 1 + 397 days ago: inside a 401-day raw retention's
    // backfill, which starts 398 days ago.
    assert!(
        days.thirteen_months >= days.oldest_401,
        "{} is before {}",
        days.thirteen_months,
        days.oldest_401
    );

    // Before 0010: the files up to it and a 401-day raw retention, as a deployment before
    // Laysi/trustiot-v2#141 has them, and raw rows received on the test's days.
    for migration in PRODUCT
        .clickhouse
        .iter()
        .filter(|migration| migration.name < "0010")
    {
        for statement in migrate::clickhouse::split_statements(migration.sql)
            .unwrap_or_else(|error| panic!("{}: {error}", migration.name))
        {
            admin(&clickhouse, &statement, &[]).await;
        }
    }
    for statement in EARLIER_RETENTION {
        admin(&clickhouse, statement, &[]).await;
    }
    let noon = |day: &str| format!("{day} 12:00:00.000");
    insert(&clickhouse, 1, &noon(&days.before_401)).await;
    insert(&clickhouse, 2, &noon(&days.thirteen_months)).await;
    insert(&clickhouse, 3, &noon(&days.month)).await;
    insert(&clickhouse, 4, &noon(&days.yesterday)).await;
    insert(&clickhouse, 5, &format!("{} 00:00:01.000", days.today)).await;

    // 1. The first run.
    let report = migrate::run(&config(&db, &clickhouse, 401), &CLICKHOUSE_ONLY)
        .await
        .expect("the first run");
    println!("first run: {:?}", report.changes);
    let backfilled = format!(
        "backfilled the one-minute and one-hour aggregates from the raw rows received from {} \
         to {} (raw data retention 401 days, less the three oldest days)",
        days.oldest_401, days.today
    );
    let ttl = report
        .changes
        .iter()
        .position(|change| {
            change == "set the retention of ClickHouse table trustiot.telemetry_1m to 14 months (was 13)"
        })
        .unwrap_or_else(|| panic!("the one-minute retention: {:?}", report.changes));
    let backfill = report
        .changes
        .iter()
        .position(|change| *change == backfilled)
        .unwrap_or_else(|| panic!("the backfill: {:?}", report.changes));
    assert!(ttl < backfill, "the TTLs are set before the backfill");
    assert_eq!(
        admin(&clickhouse, RECORDS, &[("name", BACKFILL_RECORD)]).await,
        format!("{BACKFILL_RECORD}\n")
    );
    assert_eq!(admin(&clickhouse, POLICIES, &[]).await, POLICIES_UNBOUND);

    let reader = random_hex();
    admin(&clickhouse, CREATE_READER, &[("password", reader.as_str())]).await;
    for statement in READER_ACCESS {
        admin(&clickhouse, statement, &[]).await;
    }
    // The cells of the day received 13 months and a day ago survive a merge: the run set the
    // one-minute retention to 14 months before it backfilled.
    merge(&clickhouse).await;
    let first = per_day(&clickhouse, &reader).await;
    println!("after the first run and a merge: {first:?}");
    for day in [
        &days.thirteen_months,
        &days.month,
        &days.yesterday,
        &days.today,
    ] {
        assert_cells_equal_raw(&first, day, "first run");
    }
    assert_eq!(
        on_day(&first, &days.before_401),
        [("raw".to_owned(), ROWS.to_owned())],
        "a day before the bound is not backfilled"
    );

    let before = per_day(&clickhouse, &reader).await;
    let report = migrate::run(&config(&db, &clickhouse, 401), &CLICKHOUSE_ONLY)
        .await
        .expect("the second run");
    assert!(report.changes.is_empty(), "{:?}", report.changes);
    assert_eq!(per_day(&clickhouse, &reader).await, before);

    // 2. A 200-day raw retention: the older raw rows expire. On the cut-off day, rows received
    //    at its first millisecond expire and rows received at its last stay.
    let report = migrate::run(&config(&db, &clickhouse, 200), &CLICKHOUSE_ONLY)
        .await
        .expect("the run with a 200-day raw retention");
    assert!(
        report
            .changes
            .iter()
            .all(|change| change.starts_with("set the retention")),
        "{:?}",
        report.changes
    );
    insert(
        &clickhouse,
        6,
        &format!("{} 00:00:00.000", days.cut_off_200),
    )
    .await;
    insert(
        &clickhouse,
        7,
        &format!("{} 23:59:59.999", days.cut_off_200),
    )
    .await;
    merge(&clickhouse).await;
    for statement in DELETE_CELLS {
        admin(&clickhouse, statement, &[("day", days.yesterday.as_str())]).await;
    }
    let expired = per_day(&clickhouse, &reader).await;
    println!("after the 200-day retention, a merge and the loss of yesterday's cells: {expired:?}");
    assert_eq!(
        on_day(&expired, &days.cut_off_200),
        [
            ("raw".to_owned(), ROWS.to_owned()),
            ("telemetry_1m".to_owned(), "8".to_owned()),
            ("telemetry_1h".to_owned(), "8".to_owned()),
        ],
        "the cut-off day keeps the rows of its last millisecond"
    );
    assert_eq!(
        on_day(&expired, &days.yesterday),
        [("raw".to_owned(), ROWS.to_owned())],
        "yesterday's cells are gone"
    );
    let outside = |all: &[Vec<String>]| -> Vec<Vec<String>> {
        all.iter()
            .filter(|row| row[1] < days.oldest_200)
            .cloned()
            .collect()
    };
    let outside_before = outside(&expired);
    assert!(
        outside_before
            .iter()
            .any(|row| row[0] == "telemetry_1m" && row[1] == days.thirteen_months),
        "{outside_before:?}"
    );

    // The views show nothing to the admin while no run backfills, although yesterday differs;
    // named in the policy, it sees the difference.
    let check = [("day", days.yesterday.as_str()), ("raw", "200")];
    assert_eq!(admin(&clickhouse, CHECK_DAY, &check).await, "0\n");
    admin(&clickhouse, SHOW_BACKFILL_VIEWS, &[]).await;
    let shown = admin(&clickhouse, CHECK_DAY, &check).await;
    // The runbook trusts the check only while both policies still name the account.
    let during = rows(&admin(&clickhouse, RUNBOOK_POLICIES, &[]).await);
    admin(&clickhouse, HIDE_BACKFILL_VIEWS, &[]).await;
    assert_ne!(
        shown, "0\n",
        "named in the policy, the admin sees the difference"
    );
    assert_eq!(during.len(), 2, "{during:?}");
    assert!(during.iter().all(|row| row[3] == "1"), "{during:?}");
    assert_eq!(
        admin(&clickhouse, RUNBOOK_POLICIES, &[]).await,
        "telemetry_1h_backfill\t0\t[]\t0\ntelemetry_1m_backfill\t0\t[]\t0\n",
        "the runbook's last read after TO NONE"
    );
    println!(
        "backfill views: the admin read 0 difference rows for {} while the policy named nobody, \
         {} while it named the admin; the runbook's read of the policies showed mine = 1 on both \
         during the check and `0 [] 0` on both after TO NONE",
        days.yesterday,
        shown.trim()
    );

    // The record is lost; a second administrator account runs the Job.
    admin(&clickhouse, LOSE_RECORDS, &[]).await;
    let password = random_hex();
    admin(
        &clickhouse,
        CREATE_SECOND_ADMIN,
        &[("password", password.as_str())],
    )
    .await;
    admin(&clickhouse, GRANT_SECOND_ADMIN, &[]).await;
    let mut second = config(&db, &clickhouse, 200);
    if let Some(settings) = second.clickhouse.as_mut() {
        settings.username = SECOND_ADMIN.to_owned();
        settings.password = Some(password.clone());
    }
    let report = migrate::run(&second, &CLICKHOUSE_ONLY)
        .await
        .expect("the run after the record was lost");
    println!(
        "run after the record was lost, as a second administrator: {:?}",
        report.changes
    );
    for migration in PRODUCT.clickhouse {
        let applied = format!("applied clickhouse/{}", migration.name);
        assert!(
            report.changes.contains(&applied),
            "{applied}: {:?}",
            report.changes
        );
    }
    let rebackfilled = format!(
        "backfilled the one-minute and one-hour aggregates from the raw rows received from {} \
         to {} (raw data retention 200 days, less the three oldest days)",
        days.oldest_200, days.today
    );
    assert!(
        report.changes.contains(&rebackfilled),
        "{rebackfilled}: {:?}",
        report.changes
    );
    assert_eq!(admin(&clickhouse, POLICIES, &[]).await, POLICIES_UNBOUND);

    let after = per_day(&clickhouse, &reader).await;
    println!("after the run: {after:?}");
    assert_cells_equal_raw(&after, &days.yesterday, "yesterday restored");
    for day in [&days.month, &days.today] {
        assert_cells_equal_raw(&after, day, "within the bound");
    }
    assert_eq!(
        outside(&after),
        outside_before,
        "the cells of days whose raw rows may have expired changed"
    );
    // 3. A policy left naming accounts: the next run, which has the backfill record, names
    //    nobody again and says so; the run after it changes nothing.
    for statement in LEFT_BOUND {
        admin(&clickhouse, statement, &[]).await;
    }
    let left = admin(&clickhouse, POLICIES, &[]).await;
    let left_rows = rows(&left);
    assert_eq!(left_rows.len(), 2, "{left}");
    assert_eq!(left_rows[0][..2], ["telemetry_1h_backfill", "1"], "{left}");
    assert_eq!(left_rows[1][..2], ["telemetry_1m_backfill", "0"], "{left}");
    assert_ne!(left_rows[1][2], "[]", "{left}");
    assert_eq!(
        admin(&clickhouse, RECORDS, &[("name", BACKFILL_RECORD)]).await,
        format!("{BACKFILL_RECORD}\n"),
        "the backfill is recorded"
    );
    let report = migrate::run(&config(&db, &clickhouse, 200), &CLICKHOUSE_ONLY)
        .await
        .expect("the run after a policy was left naming accounts");
    let reset = format!(
        "set the migration_account row policies of trustiot.telemetry_1h_backfill (all), \
         trustiot.telemetry_1m_backfill ({}) back to name nobody",
        left_rows[1][2]
    );
    assert_eq!(report.changes, std::slice::from_ref(&reset));
    assert_eq!(admin(&clickhouse, POLICIES, &[]).await, POLICIES_UNBOUND);
    let report = migrate::run(&config(&db, &clickhouse, 200), &CLICKHOUSE_ONLY)
        .await
        .expect("the run after that");
    assert!(report.changes.is_empty(), "{:?}", report.changes);
    assert_eq!(admin(&clickhouse, POLICIES, &[]).await, POLICIES_UNBOUND);
    println!(
        "policies left naming accounts ({}): the next run with the backfill recorded reported \
         \"{reset}\" and left both naming nobody; the run after it reported no change",
        left.trim().replace('\n', "; ").replace('\t', " ")
    );

    let today_now = rows(&admin(&clickhouse, DAYS, &[]).await)
        .pop()
        .expect("the days")
        .swap_remove(0);
    assert_eq!(
        today_now, days.today,
        "the UTC date changed during the test, which names days by their distance from today: \
         run it again"
    );
    println!(
        "lost record: the run as {SECOND_ADMIN} restored the cells of {}, left the {} rows of \
         days before {} as they were (the cut-off day {} still counts 8 readings, its raw rows \
         4), and the backfill views show nothing again",
        days.yesterday,
        outside_before.len(),
        days.oldest_200,
        days.cut_off_200
    );
}
