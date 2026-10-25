//! The AI SQL check (Laysi/trustiot-v2#143) against statements that change a schema. They live
//! here rather than with the check's unit tests in `src/sql/` because repo-check rejects schema
//! changes written anywhere in `services/*/src`, test inputs included.
//!
//! No container: the check is pure, and a refused statement never reaches ClickHouse.

use contract::api::telemetry_query::SqlRefusal;
use telemetry_query::sql::{CheckError, check_on_thread};

async fn refusal(sql: &str) -> SqlRefusal {
    match check_on_thread(sql.to_owned(), 200).await {
        Err(CheckError::Refused(refusal)) => refusal,
        other => panic!("{sql}: expected a refusal, got {other:?}"),
    }
}

#[tokio::test]
async fn a_schema_change_is_never_a_query() {
    for sql in [
        "ALTER TABLE trustiot.telemetry DELETE WHERE 1",
        "ALTER TABLE trustiot.telemetry_1m UPDATE value_sum = 0 WHERE 1",
        "TRUNCATE TABLE trustiot.telemetry",
        "DROP TABLE trustiot.telemetry",
        "DROP VIEW trustiot.tenant_telemetry",
        "DROP DATABASE trustiot",
        "DROP DICTIONARY trustiot.d",
        "CREATE TABLE t (x UInt8) ENGINE = Memory",
        "CREATE OR REPLACE VIEW trustiot.tenant_telemetry AS SELECT 1",
        "CREATE DATABASE other",
        "RENAME TABLE trustiot.telemetry TO trustiot.gone",
        "EXCHANGE TABLES trustiot.telemetry AND trustiot.telemetry_1m",
        "ATTACH TABLE trustiot.telemetry",
        "DETACH TABLE trustiot.telemetry",
        "UNDROP TABLE trustiot.telemetry",
        "  drop table t",
        "/* a comment */ ALTER TABLE t DROP COLUMN x",
    ] {
        assert_eq!(refusal(sql).await, SqlRefusal::NotASelect, "{sql}");
    }
}

#[tokio::test]
async fn a_schema_change_never_follows_a_query() {
    for sql in [
        "(SELECT 1); DROP TABLE t",
        "SELECT 1 FROM tenant_telemetry; DROP TABLE trustiot.telemetry",
        "SELECT 1 FROM tenant_telemetry;\nTRUNCATE TABLE trustiot.telemetry",
        "SELECT 1 FROM tenant_telemetry; -- trailing\nALTER TABLE trustiot.telemetry DELETE WHERE 1",
    ] {
        assert_eq!(refusal(sql).await, SqlRefusal::MultipleStatements, "{sql}");
    }
}

#[tokio::test]
async fn a_schema_change_in_a_comment_is_dropped_from_what_runs() {
    for sql in [
        "SELECT 1 FROM tenant_telemetry -- ; DROP TABLE trustiot.telemetry",
        "SELECT 1 /* ; TRUNCATE TABLE trustiot.telemetry */ FROM tenant_telemetry",
    ] {
        let checked = check_on_thread(sql.to_owned(), 200)
            .await
            .unwrap_or_else(|error| panic!("{sql}: {error:?}"));
        assert_eq!(
            checked.canonical, "SELECT 1 FROM trustiot.tenant_telemetry",
            "{sql}"
        );
        for text in [&checked.canonical, &checked.statement] {
            assert!(!text.contains("DROP"), "{sql}: {text}");
            assert!(!text.contains("TRUNCATE"), "{sql}: {text}");
            assert!(!text.contains("--"), "{sql}: {text}");
            assert!(!text.contains("/*"), "{sql}: {text}");
        }
    }
}

#[tokio::test]
async fn a_schema_change_as_a_string_stays_a_string() {
    let sql = "SELECT 'DROP TABLE trustiot.telemetry' AS text FROM tenant_telemetry";
    let checked = check_on_thread(sql.to_owned(), 200)
        .await
        .unwrap_or_else(|error| panic!("{sql}: {error:?}"));
    assert_eq!(
        checked.canonical,
        "SELECT 'DROP TABLE trustiot.telemetry' AS text FROM trustiot.tenant_telemetry"
    );
}
