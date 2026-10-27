//! The attack types of the 2026-10-09 PoC (decision 14), as a tenant's account would run them:
//! `tests/tenant_accounts.rs` runs them as the account directly (Laysi/trustiot-v2#119), and
//! `tests/sql_api.rs` through the SQL endpoint (Laysi/trustiot-v2#143).

/// A tenant that exists only in ClickHouse: its readings are written directly, as if it were
/// another customer. Its ID is a constant so the attacks can name it in SQL text.
pub const VICTIM: &str = "0199a0b0-c0d0-7e0f-8a1b-2c3d4e5f6a7b";
/// What every text reading of the victim, and of tenant B, starts with.
pub const VICTIM_MARKER: &str = "victim-marker";
pub const B_MARKER: &str = "tenant-b-marker";
pub const A_MARKER: &str = "tenant-a-marker";

/// One attack: a name, the statement, and settings sent in the URL.
pub struct Attack {
    pub name: &'static str,
    pub sql: &'static str,
    pub url: &'static [(&'static str, &'static str)],
}

pub const fn attack(name: &'static str, sql: &'static str) -> Attack {
    Attack {
        name,
        sql,
        url: &[],
    }
}

/// The PoC's attack types, as tenant A's account. Each must be refused or answer without a
/// reading of another tenant.
pub const ATTACKS: &[Attack] = &[
    // Direct reads of the base tables, and filters on the view.
    attack("base table", "SELECT value_string FROM trustiot.telemetry"),
    attack("batch commits", "SELECT * FROM trustiot.batch_commits"),
    attack("stream commits", "SELECT * FROM trustiot.stream_commits"),
    attack(
        "view filtered to another tenant",
        "SELECT value_string FROM trustiot.tenant_telemetry WHERE tenant_id = '0199a0b0-c0d0-7e0f-8a1b-2c3d4e5f6a7b'",
    ),
    attack(
        "view with an always-true filter",
        "SELECT value_string FROM trustiot.tenant_telemetry WHERE 1 OR tenant_id != tenant_id",
    ),
    attack(
        "explicit PREWHERE",
        "SELECT value_string FROM trustiot.tenant_telemetry PREWHERE tenant_id = '0199a0b0-c0d0-7e0f-8a1b-2c3d4e5f6a7b'",
    ),
    attack(
        "FINAL",
        "SELECT value_string FROM trustiot.tenant_telemetry FINAL",
    ),
    // Joins, subqueries and set operations that reach the base table.
    attack(
        "join with the base table",
        "SELECT t.value_string FROM trustiot.tenant_telemetry AS v JOIN trustiot.telemetry AS t ON 1",
    ),
    attack(
        "scalar subquery",
        "SELECT (SELECT groupArray(value_string) FROM trustiot.telemetry)",
    ),
    attack(
        "IN subquery",
        "SELECT value_string FROM trustiot.tenant_telemetry WHERE device_id IN (SELECT device_id FROM trustiot.telemetry)",
    ),
    attack(
        "UNION ALL",
        "SELECT value_string FROM trustiot.tenant_telemetry UNION ALL SELECT value_string FROM trustiot.telemetry",
    ),
    attack(
        "common table expression",
        "WITH x AS (SELECT value_string FROM trustiot.telemetry) SELECT * FROM x",
    ),
    // Table functions.
    attack(
        "merge()",
        "SELECT value_string FROM merge('trustiot', '^tele')",
    ),
    attack(
        "view()",
        "SELECT * FROM view(SELECT value_string FROM trustiot.telemetry)",
    ),
    attack(
        "remote() to the same server",
        "SELECT value_string FROM remote('127.0.0.1:9000', trustiot.telemetry)",
    ),
    attack(
        "clusterAllReplicas()",
        "SELECT value_string FROM clusterAllReplicas('default', trustiot.telemetry)",
    ),
    attack(
        "url() back to the HTTP interface",
        "SELECT * FROM url('http://127.0.0.1:8123/?query=SELECT+value_string+FROM+trustiot.telemetry', 'LineAsString')",
    ),
    attack(
        "file()",
        "SELECT * FROM file('/etc/passwd', 'LineAsString')",
    ),
    // Another tenant through the setting, and the limits through settings.
    attack(
        "SETTINGS clause with another tenant",
        "SELECT value_string FROM trustiot.tenant_telemetry SETTINGS SQL_tenant_id = '0199a0b0-c0d0-7e0f-8a1b-2c3d4e5f6a7b'",
    ),
    Attack {
        name: "URL setting with another tenant",
        sql: "SELECT value_string FROM trustiot.tenant_telemetry",
        url: &[("SQL_tenant_id", VICTIM)],
    },
    attack(
        "SET another tenant",
        "SET SQL_tenant_id = '0199a0b0-c0d0-7e0f-8a1b-2c3d4e5f6a7b'",
    ),
    attack(
        "SETTINGS readonly = 0",
        "SELECT value_string FROM trustiot.tenant_telemetry SETTINGS readonly = 0",
    ),
    attack(
        "SETTINGS without the row limit",
        "SELECT count() FROM numbers(2000000000) SETTINGS max_rows_to_read = 0",
    ),
    Attack {
        name: "URL setting without the time limit",
        sql: "SELECT 1",
        url: &[("max_execution_time", "0")],
    },
    // Other identities.
    attack(
        "EXECUTE AS the view's definer",
        "EXECUTE AS trustiot_tenant_view_definer SELECT value_string FROM trustiot.telemetry",
    ),
    Attack {
        name: "URL role of the ingest service",
        sql: "SELECT value_string FROM trustiot.telemetry",
        url: &[("role", "trustiot_ingest")],
    },
    // Writes and access management.
    attack(
        "insert into the base table",
        "INSERT INTO trustiot.telemetry (tenant_id, point, value_type, value_string) VALUES ('0199a0b0-c0d0-7e0f-8a1b-2c3d4e5f6a7b', 'x', 'string', 'written')",
    ),
    attack(
        "delete from the base table",
        "ALTER TABLE trustiot.telemetry DELETE WHERE 1",
    ),
    attack(
        "truncate the base table",
        "TRUNCATE TABLE trustiot.telemetry",
    ),
    attack(
        "create a table",
        "CREATE TABLE default.copy ENGINE = Memory AS SELECT * FROM trustiot.tenant_telemetry",
    ),
    attack(
        "create a tenant account",
        "CREATE USER trustiot_tenant_account_ffffffffffffffffffffffffffffffff",
    ),
    attack(
        "change the service's account",
        "ALTER USER trustiot_telemetry_query IDENTIFIED WITH no_password",
    ),
    attack(
        "grant the base table",
        "GRANT SELECT ON trustiot.telemetry TO trustiot_tenant_reader",
    ),
    attack(
        "drop the row policy",
        "DROP ROW POLICY tenant_isolation ON trustiot.tenant_telemetry",
    ),
];

/// System tables that would show other tenants' queries, readings or accounts: each must be
/// refused for want of the privilege (`ACCESS_DENIED`), after `SYSTEM FLUSH LOGS` has created
/// the log tables, so a missing table cannot pass for a refusal.
pub const SYSTEM_TABLES: &[&str] = &[
    "SELECT count() FROM system.query_log",
    "SELECT count() FROM system.text_log",
    "SELECT count() FROM system.processes",
    "SELECT count() FROM system.parts",
    "SELECT count() FROM system.users",
    "SELECT count() FROM system.role_grants",
    "SELECT count() FROM system.settings_profile_elements",
];

/// Heavy queries the settings profile stops (0004_access.sql), with the error each must end in.
pub const HEAVY: &[(&str, &str, &str)] = &[
    (
        "reads more than max_rows_to_read",
        "SELECT count() FROM numbers(2000000000)",
        "TOO_MANY_ROWS",
    ),
    (
        "holds more than max_memory_usage",
        "SELECT length(groupArray(toString(number))) FROM numbers(200000000)",
        "MEMORY_LIMIT_EXCEEDED",
    ),
    (
        "runs longer than max_execution_time",
        "SELECT count() FROM numbers_mt(999000000) WHERE NOT ignore(SHA256(SHA256(SHA256(toString(number)))))",
        "TIMEOUT_EXCEEDED",
    ),
];
