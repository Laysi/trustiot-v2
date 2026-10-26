//! The migration Job's settings as the binary loads them, started the way the chart starts it:
//! a raw data retention below the minimum ends `migrate run` with exit code 2 before it connects
//! to any database (docs/runbooks/migrations.md, 「原始資料的保存期限」). No containers needed: the
//! database addresses point nowhere, and a refused setting never reaches them.

use std::process::{Command, Output, Stdio};

use migrate::retention::{MIN_RAW_RETENTION_DAYS, RECONCILE_WINDOW_DAYS};
use protocol::telemetry::BUFFER_MAX_AGE_DAYS;

const BINARY: &str = env!("CARGO_BIN_EXE_migrate");

/// `migrate run` with the settings the chart gives it and `days` as the raw data retention. The
/// databases do not exist and the wait for them is short, so a setting the run wrongly accepts
/// ends it quickly with another exit code instead of waiting.
fn run(days: &str) -> Output {
    Command::new(BINARY)
        .env_clear()
        .arg("run")
        .env("TRUSTIOT_POSTGRES_HOST", "postgres.invalid")
        .env("TRUSTIOT_POSTGRES_PORT", "5432")
        .env("TRUSTIOT_POSTGRES_DATABASE", "trustiot")
        .env("TRUSTIOT_POSTGRES_SSLMODE", "verify-full")
        .env("TRUSTIOT_POSTGRES_CA_FILE", "/nonexistent/ca.crt")
        .env("TRUSTIOT_CLICKHOUSE_URL", "http://clickhouse.invalid:8123")
        .env("TRUSTIOT__POSTGRES__USERNAME", "trustiot_owner")
        .env("TRUSTIOT__POSTGRES__PASSWORD", "unused")
        .env("TRUSTIOT__CLICKHOUSE__USERNAME", "trustiot_admin")
        .env("TRUSTIOT__CLICKHOUSE__PASSWORD", "unused")
        .env("TRUSTIOT__WAIT__DATABASES", "1s")
        .env("TRUSTIOT__CLICKHOUSE__RAW_RETENTION_DAYS", days)
        .stdin(Stdio::null())
        .output()
        .expect("run migrate")
}

#[test]
fn a_raw_retention_below_the_minimum_exits_with_code_2() {
    assert_eq!(MIN_RAW_RETENTION_DAYS, 33);
    for days in ["32", "31", "30"] {
        let output = run(days);
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.status.code(), Some(2), "{days} days: {text}");
        for part in [
            "clickhouse.raw_retention_days".to_owned(),
            format!("maximum age of {BUFFER_MAX_AGE_DAYS} days"),
            format!("reconcile's window of {RECONCILE_WINDOW_DAYS} receive days"),
            format!("use at least {MIN_RAW_RETENTION_DAYS} days"),
        ] {
            assert!(text.contains(&part), "{days} days, {part}: {text}");
        }
        let line = text
            .lines()
            .find(|line| line.contains("clickhouse.raw_retention_days"))
            .unwrap_or_default();
        println!("{days} days: exit code 2: {line}");
    }
}
