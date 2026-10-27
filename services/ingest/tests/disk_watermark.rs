//! The disk watermark alert (rebuild checklist I2, Laysi/trustiot-v2#92): a real ClickHouse whose
//! data directory is a small tmpfs, filled from inside the container past each threshold of
//! `alerts/clickhouse_disk_watermark.sql`, evaluated as an account with the monitoring role of
//! `0007_monitoring.sql`, which reads nothing else.
//!
//! Needs podman or docker (always a throwaway container: `TEST_CLICKHOUSE_URL` is not used,
//! because a shared server's disk is not the test's to fill); skipped without one unless `CI` is
//! set.

mod common;

use common::{Account, ClickHouse};

const RULE: &str = include_str!("../alerts/clickhouse_disk_watermark.sql");

/// The data directory's size. Small, so the test fills it with a few hundred MB at most; large
/// enough for ClickHouse's own system log tables during the test.
const DISK_SIZE: &str = "256m";
const DATA_DIR: &str = "/var/lib/clickhouse";

/// The monitoring account, as the migration Job would create Grafana's (Laysi/trustiot-v2#183).
/// No password: the container is local and thrown away after the test.
const CREATE_MONITOR: &str =
    "CREATE USER watermark_monitor IDENTIFIED WITH no_password DEFAULT ROLE trustiot_monitoring";
const GRANT_MONITOR: &str = "GRANT trustiot_monitoring TO watermark_monitor";
const CREATE_PLAIN: &str = "CREATE USER watermark_plain IDENTIFIED WITH no_password";
const MONITOR_GRANTS: &str = "SELECT access_type, database, table, grant_option, is_partial_revoke
FROM system.grants WHERE role_name = 'trustiot_monitoring' FORMAT TSV";
const MONITOR_ROLES: &str =
    "SELECT count() FROM system.role_grants WHERE role_name = 'trustiot_monitoring' FORMAT TSV";

/// One row of the rule's answer.
#[derive(Debug)]
struct Disk {
    disk: String,
    total_bytes: u64,
    free_bytes: u64,
    used_ratio: f64,
    severity: String,
}

/// The rule as a statement: the file without its comment lines, asking for one JSON object per
/// row.
fn rule() -> String {
    let body: Vec<&str> = RULE
        .lines()
        .filter(|line| !line.trim_start().starts_with("--"))
        .collect();
    let statement = body.join("\n");
    let statement = statement.trim();
    assert!(
        !statement.contains(';'),
        "the rule is one statement without a semicolon"
    );
    format!("{statement}\nFORMAT JSONEachRow")
}

fn number(row: &serde_json::Value, column: &str) -> u64 {
    // ClickHouse quotes 64-bit integers in JSON output by default.
    match &row[column] {
        serde_json::Value::String(text) => text.parse().expect("an integer"),
        value => value.as_u64().expect("an integer"),
    }
}

async fn evaluate(ch: &ClickHouse, account: &Account) -> Vec<Disk> {
    let answer = ch
        .query(account, &rule())
        .await
        .unwrap_or_else(|error| panic!("the rule failed: {}", error.body));
    answer
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            let row: serde_json::Value =
                serde_json::from_str(line).unwrap_or_else(|error| panic!("{error}: {line}"));
            Disk {
                disk: row["disk"].as_str().expect("disk").to_owned(),
                total_bytes: number(&row, "total_bytes"),
                free_bytes: number(&row, "free_bytes"),
                used_ratio: row["used_ratio"].as_f64().expect("used_ratio"),
                severity: row["severity"].as_str().expect("severity").to_owned(),
            }
        })
        .collect()
}

/// The data disk: the only local disk of this server.
async fn data_disk(ch: &ClickHouse, account: &Account) -> Disk {
    let mut disks = evaluate(ch, account).await;
    assert_eq!(disks.len(), 1, "one local disk: {disks:?}");
    let disk = disks.remove(0);
    assert_eq!(disk.disk, "default");
    disk
}

/// Fills the data disk with a file of the size that brings it to `ratio` in use.
fn fill_to(ch: &ClickHouse, disk: &Disk, ratio: f64, file: &str) {
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss,
        reason = "byte counts of a 256 MiB disk are far inside f64's exact integer range"
    )]
    let target_used = (disk.total_bytes as f64 * ratio) as u64;
    let used = disk.total_bytes - disk.free_bytes;
    let bytes = target_used.saturating_sub(used);
    assert!(bytes > 0, "the disk is already past {ratio}: {disk:?}");
    let path = format!("{DATA_DIR}/{file}");
    ch.exec(&["fallocate", "-l", &bytes.to_string(), &path]);
}

#[tokio::test(flavor = "current_thread")]
async fn the_disk_watermark_alert_fires_past_each_threshold() {
    let Some(ch) = ClickHouse::start_with_small_disk(DISK_SIZE).await else {
        return;
    };
    ch.apply_migrations().await;
    ch.admin_ok(CREATE_MONITOR).await;
    ch.admin_ok(GRANT_MONITOR).await;
    ch.admin_ok(CREATE_PLAIN).await;
    let monitor = Account {
        user: "watermark_monitor".to_owned(),
        password: String::new(),
    };
    let plain = Account {
        user: "watermark_plain".to_owned(),
        password: String::new(),
    };
    let mut evidence = Vec::new();

    // 1. A nearly empty disk: ok.
    let empty = data_disk(&ch, &monitor).await;
    assert_eq!(empty.severity, "ok", "{empty:?}");
    assert!(empty.used_ratio < 0.80, "{empty:?}");
    evidence.push(format!("empty: {empty:?}"));

    // 2. Past 80 %: warning.
    fill_to(&ch, &empty, 0.85, "watermark-fill-1");
    let warning = data_disk(&ch, &monitor).await;
    assert_eq!(warning.severity, "warning", "{warning:?}");
    assert!((0.80..0.90).contains(&warning.used_ratio), "{warning:?}");
    evidence.push(format!("filled to 85 %: {warning:?}"));

    // 3. Past 90 %: critical.
    fill_to(&ch, &warning, 0.95, "watermark-fill-2");
    let critical = data_disk(&ch, &monitor).await;
    assert_eq!(critical.severity, "critical", "{critical:?}");
    assert!(critical.used_ratio >= 0.90, "{critical:?}");
    evidence.push(format!("filled to 95 %: {critical:?}"));

    // 4. Space freed: ok again.
    ch.exec(&[
        // The files exist: a plain rm fails loudly if they do not.
        "rm",
        "--",
        &format!("{DATA_DIR}/watermark-fill-1"),
        &format!("{DATA_DIR}/watermark-fill-2"),
    ]);
    let freed = data_disk(&ch, &monitor).await;
    assert_eq!(freed.severity, "ok", "{freed:?}");
    evidence.push(format!("freed: {freed:?}"));

    // 5. The monitoring role reads system.disks and nothing else; without it, system.disks is
    //    refused.
    let grants = ch.admin_ok(MONITOR_GRANTS).await;
    assert_eq!(grants, "SELECT\tsystem\tdisks\t0\t0\n");
    assert_eq!(ch.admin_ok(MONITOR_ROLES).await.trim(), "0");
    for sql in [
        "SELECT count() FROM trustiot.telemetry",
        "SELECT count() FROM trustiot.batch_commits",
        "SELECT count() FROM trustiot.stream_commits",
        "SELECT count() FROM trustiot.tenant_telemetry",
        "SELECT count() FROM trustiot_logs.otel_logs",
        "INSERT INTO trustiot_logs.otel_logs (Body) VALUES ('injected')",
        "ALTER TABLE trustiot.telemetry MODIFY TTL toDateTime(received_at) + toIntervalDay(1)",
        "CREATE USER watermark_escalate IDENTIFIED WITH no_password",
    ] {
        let error = ch
            .query(&monitor, sql)
            .await
            .expect_err("the monitoring role reads system.disks only");
        assert_eq!(error.code(), "ACCESS_DENIED", "{sql}: {}", error.body);
    }
    let error = ch
        .query(&plain, &rule())
        .await
        .expect_err("system.disks needs the grant");
    assert_eq!(error.code(), "ACCESS_DENIED", "{}", error.body);

    #[allow(clippy::print_stdout, reason = "test evidence")]
    {
        println!(
            "disk watermark rule, step by step:\n{}",
            evidence.join("\n")
        );
        println!("monitoring role grants: {}", grants.trim());
    }
}
