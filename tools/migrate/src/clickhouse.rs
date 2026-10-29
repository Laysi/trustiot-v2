//! The ClickHouse part of a run: the DDL of `services/ingest/migrations/clickhouse/` and the
//! accounts of the OpenTelemetry collector, of telemetry-query and of ingest, over the HTTP
//! interface with the access-management account.
//!
//! Every statement of those files is idempotent (services/ingest/README.md, "檔案格式"), but a
//! later `ALTER TABLE ... MODIFY TTL` would start a mutation on every run, so the run also
//! records each applied file in `trustiot_migrate.applied` in ClickHouse itself: a ClickHouse
//! restored or rebuilt separately from Postgres gets its files again. A file that failed halfway
//! is not recorded, and the next run repeats it from the start, which its idempotent statements
//! allow.
//!
//! After the files and the data retention, a run without a record of it fills the one-minute
//! and one-hour aggregates from the raw rows the telemetry table already held
//! (`ClickHouse::backfill_aggregates`), and every run then makes sure the backfill views show
//! their rows to nobody (`ClickHouse::hide_backfill_views`).
//!
//! Values (names, hashes) travel as query parameters (`{name:Type}`), never inside the SQL text.
//! ClickHouse 26.8 takes parameters in `CREATE USER` and `ALTER USER` but not in `GRANT` or
//! `DROP USER`, so the collector's and ingest's roles come with `CREATE USER ... DEFAULT ROLE`,
//! which grants them, and an account with other grants is reported for an administrator to drop.

use std::time::{Duration, Instant};

use crate::config::{
    ClickHouseSettings, INGEST_ACCOUNT, RunConfig, TELEMETRY_QUERY_ACCOUNT, TENANT_ACCOUNT_PREFIX,
};
use crate::passwords::Api;
use crate::registry::{Migration, Registry};
use crate::retention::{MAX_RAW_RETENTION_DAYS, Retention};
use crate::rollup::{BACKFILL_1H, BACKFILL_1M};
use crate::secret::Password;
use crate::{Error, Report, clickhouse_admin, http, sha256_hex};

/// The role `0006_logs.sql` creates for the collector: INSERT and SHOW COLUMNS on the two log
/// tables, nothing else.
pub const COLLECTOR_ROLE: &str = "trustiot_otel_collector";

/// The ingest service's role (`0004_access.sql`): what the DDL files grant it
/// ([`INGEST_EXPECTED_GRANTS`]), nothing else. The run gives it to [`INGEST_ACCOUNT`] alone.
pub const INGEST_ROLE: &str = "trustiot_ingest";

/// The tenant role of decision 14 (`0004_access.sql`): SELECT on the tenant views, nothing else.
pub const TENANT_READER_ROLE: &str = "trustiot_tenant_reader";

/// The views the tenant role reads, and nothing else: the raw readings (`0005_tenant_view.sql`)
/// and the one-minute and one-hour aggregates (`0010_telemetry_rollups.sql`).
pub const TENANT_VIEWS: [&str; 3] = [
    "tenant_telemetry",
    "tenant_telemetry_1m",
    "tenant_telemetry_1h",
];

const RETRY_EVERY: Duration = Duration::from_secs(2);
const REPORT_EVERY: Duration = Duration::from_secs(30);
/// Error text from ClickHouse is cut to this many characters in messages.
const ERROR_TEXT_LIMIT: usize = 500;

const BOOTSTRAP: [&str; 2] = [
    "CREATE DATABASE IF NOT EXISTS trustiot_migrate",
    "CREATE TABLE IF NOT EXISTS trustiot_migrate.applied (
    name String,
    sha256 FixedString(64),
    applied_at DateTime64(3, 'UTC') DEFAULT now64(3)
) ENGINE = MergeTree ORDER BY name",
];
const APPLIED: &str =
    "SELECT DISTINCT sha256 FROM trustiot_migrate.applied WHERE name = {name:String} FORMAT TSV";
const RECORD: &str =
    "INSERT INTO trustiot_migrate.applied (name, sha256) SELECT {name:String}, {sha256:String}";
/// The row of `trustiot_migrate.applied` that records the backfill of the aggregates
/// (`ClickHouse::backfill_aggregates`), with the SHA-256 of its two statements. Not a file
/// name, so the record stays the same when the file that creates the aggregates is renumbered.
pub const BACKFILL_RECORD: &str = "backfill/telemetry_rollups";
const RECORDED: &str =
    "SELECT count() FROM trustiot_migrate.applied WHERE name = {name:String} FORMAT TSV";
/// The backfill views show their rows to the account their `migration_account` policy names,
/// nobody outside a backfill (0010_telemetry_rollups.sql): the run names its own account while
/// it backfills, then nobody again.
const SHOW_BACKFILL_VIEWS: &str = "ALTER ROW POLICY migration_account
ON trustiot.telemetry_1m_backfill, trustiot.telemetry_1h_backfill TO CURRENT_USER";
const HIDE_BACKFILL_VIEWS: &str = "ALTER ROW POLICY migration_account
ON trustiot.telemetry_1m_backfill, trustiot.telemetry_1h_backfill TO NONE";
/// The backfill views' `migration_account` policies that name an account, a role or everyone
/// (`TO ALL`, with or without `EXCEPT`): the table, whether the policy applies to all, and the
/// accounts and roles it lists. Outside a backfill there is none.
const BOUND_BACKFILL_POLICIES: &str = "SELECT table, apply_to_all, apply_to_list
FROM system.row_policies
WHERE database = 'trustiot' AND short_name = 'migration_account'
    AND table IN ('telemetry_1m_backfill', 'telemetry_1h_backfill')
    AND (apply_to_all OR notEmpty(apply_to_list))
ORDER BY table FORMAT TSV";
/// The newest `count` receive days up to today (UTC), oldest first, by the server's clock, as
/// the backfill views count them ([`backfill_day_count`]).
const BACKFILL_DAYS: &str = "SELECT toString(toDate(now('UTC')) - offset)
FROM (SELECT arrayJoin(range({count:UInt32})) AS offset)
ORDER BY offset DESC FORMAT TSV";
/// The last day of the newest backfill step: the last a Date holds, so a row whose receive time
/// is ahead of the server's clock is not left out.
const LAST_DAY: &str = "2149-06-06";
/// `max_execution_time` of each backfill statement, in seconds: below [`http::REQUEST_TIMEOUT`],
/// so a statement that runs too long stops on the server too, and the next run, which inserts
/// what is still missing, never runs alongside it.
const BACKFILL_MAX_SECONDS: u64 = 25;
const _: () = assert!(BACKFILL_MAX_SECONDS < http::REQUEST_TIMEOUT.as_secs());

/// The policies [`BOUND_BACKFILL_POLICIES`] read, for a message: each view with whom its policy
/// names (`all`, or the list as ClickHouse prints it).
fn bound_policies(rows: &str) -> String {
    let described: Vec<String> = rows
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            let mut fields = line.splitn(3, '\t');
            let table = fields.next().unwrap_or_default();
            let all = fields.next().unwrap_or_default() == "1";
            let list = fields.next().unwrap_or_default();
            if all {
                format!("trustiot.{table} (all)")
            } else {
                format!("trustiot.{table} ({list})")
            }
        })
        .collect();
    cut(&described.join(", "))
}

/// How many receive days the run backfills with a raw data retention of `raw_days`: from today -
/// `raw_days` + 3 to today, one day inside the backfill views' bound (today - `raw_days` + 2,
/// with the retention capped at [`MAX_RAW_RETENTION_DAYS`] as the views cap it). Each statement
/// takes the bound from the server's clock when it starts, so a midnight (UTC) during the run
/// moves the bound a day; the spare day keeps every listed day inside it, and the one-minute and
/// one-hour statements of a day always cover the same cells. The bound moves one day per
/// midnight, and the run moves on by a day at least every two request timeouts, so no later
/// midnight catches up with it.
#[must_use]
pub fn backfill_day_count(raw_days: u32) -> u32 {
    raw_days.min(MAX_RAW_RETENTION_DAYS).saturating_sub(2)
}
const USER_EXISTS: &str = "SELECT count() FROM system.users WHERE name = {user:String} FORMAT TSV";
/// The roles granted to the user, whether each is a default role (active at login) and whether it
/// came with ADMIN OPTION, then its direct grants: the account must have the collector role, as a
/// default role without ADMIN OPTION, and nothing else.
const USER_ROLES: &str = "SELECT granted_role_name, granted_role_is_default, with_admin_option
FROM system.role_grants WHERE user_name = {user:String} ORDER BY granted_role_name FORMAT TSV";
const USER_GRANTS: &str =
    "SELECT count() FROM system.grants WHERE user_name = {user:String} FORMAT TSV";
/// The collector role's grants beyond 0006_logs.sql (INSERT and SHOW COLUMNS on the two log
/// tables), plus roles granted to it: both must be 0, or the account check above is moot.
const ROLE_EXTRAS: &str = "SELECT
  (SELECT count() FROM system.grants WHERE role_name = {role:String} AND NOT (
     database = 'trustiot_logs' AND table IN ('otel_logs', 'otel_traces')
     AND access_type IN ('INSERT', 'SHOW COLUMNS') AND grant_option = 0 AND is_partial_revoke = 0))
  + (SELECT count() FROM system.role_grants WHERE role_name = {role:String})
FORMAT TSV";
/// Settings or a settings profile on the account (for example readonly = 0).
const USER_SETTINGS: &str = "SELECT count() FROM system.settings_profile_elements WHERE user_name = {user:String} FORMAT TSV";
const CREATE_COLLECTOR: &str = "CREATE USER {user:Identifier}
IDENTIFIED WITH sha256_hash BY {hash:String} SALT {salt:String}
DEFAULT ROLE trustiot_otel_collector";
const SET_PASSWORD: &str =
    "ALTER USER {user:Identifier} IDENTIFIED WITH sha256_hash BY {hash:String} SALT {salt:String}";
const PROBE: &str = "SELECT 1 FORMAT TSV";
/// The accounts the Job made for the collector, past and present: every account granted the
/// collector role. Only the Job's `CREATE USER ... DEFAULT ROLE` grants it (an administrator who
/// grants it by hand makes that account one the Job manages). `system.role_grants` has one row
/// per grant, with the grantee in `user_name` (or `role_name` for a role) and the granted role in
/// `granted_role_name`, as `USER_ROLES` reads it; tests/clickhouse.rs (step 6d) runs this against
/// the pinned ClickHouse 26.8 image.
/// Never the account the Job runs as (`currentUser()`), whatever it was granted: locking it would
/// stop every later run. ClickHouse has no marker for accounts a client created, so any other
/// account granted the collector role counts as the Job's (docs/runbooks/migrations.md: do not
/// grant it to your own accounts). Never an account defined in the server's configuration
/// (`users_xml` storage, which the Job never creates): `ALTER USER` cannot change one, so the run
/// would stop on it every time.
const COLLECTOR_ACCOUNTS: &str = "SELECT DISTINCT user_name FROM system.role_grants
WHERE granted_role_name = {role:String} AND user_name IS NOT NULL AND user_name != currentUser()
  AND user_name IN (SELECT name FROM system.users WHERE storage != 'users_xml')
ORDER BY user_name FORMAT TSV";
/// The tenant role's grants beyond SELECT on the tenant views plus roles granted to it, which
/// must be 0, and the tenant views it may SELECT, comma-separated, which must be all of
/// [`TENANT_VIEWS`]. telemetry-query grants this role to every tenant account it creates, so
/// anything added to the role would reach every tenant (services/ingest/README.md, "租戶隔離"),
/// and without a SELECT no tenant can read that view.
const READER_GRANTS: &str = "SELECT
  (SELECT count() FROM system.grants WHERE role_name = {role:String} AND NOT (
     database = 'trustiot' AND has({views:Array(String)}, ifNull(table, ''))
     AND access_type = 'SELECT' AND column IS NULL AND grant_option = 0
     AND is_partial_revoke = 0))
  + (SELECT count() FROM system.role_grants WHERE role_name = {role:String}),
  (SELECT arrayStringConcat(arraySort(groupUniqArray(ifNull(table, ''))), ',')
   FROM system.grants WHERE role_name = {role:String}
     AND database = 'trustiot' AND has({views:Array(String)}, ifNull(table, ''))
     AND access_type = 'SELECT' AND column IS NULL AND grant_option = 0
     AND is_partial_revoke = 0)
FORMAT TSV";
/// Who holds the tenant role besides those allowed to: telemetry-query's account WITH ADMIN
/// OPTION, and accounts named trustiot_tenant_account_* without it. Anyone who may grant the
/// role could have given it to anyone (telemetry-query's account only while it has GRANTEES
/// other than NONE, 0009_telemetry_query_grantees.sql); the run lists every other grantee.
const READER_GRANTEES: &str = "SELECT if(user_name IS NULL, concat('role ', role_name), user_name)
FROM system.role_grants
WHERE granted_role_name = {role:String}
  AND NOT (user_name IS NOT NULL AND user_name = {service:String} AND with_admin_option = 1)
  AND NOT (user_name IS NOT NULL AND startsWith(user_name, {prefix:String}) AND with_admin_option = 0)
ORDER BY 1 FORMAT TSV";
/// Whether telemetry-query's account still has `GRANTEES NONE` (0009_telemetry_query_grantees.sql):
/// otherwise ADMIN OPTION lets the account grant the tenant role to, or revoke it from, any
/// existing user or role.
const TELEMETRY_QUERY_GRANTEES_NONE: &str = "SELECT grantees_any = 0 AND empty(grantees_list)
FROM system.users WHERE name = {user:String} FORMAT TSV";
/// How many unexpected grantees an error names.
const GRANTEES_SHOWN: usize = 5;
/// telemetry-query's direct grants: `0008_telemetry_query.sql` gives it exactly CREATE USER and
/// ALTER USER on the tenant accounts' name prefix, without GRANT OPTION.
const TELEMETRY_QUERY_GRANTS: &str = "SELECT access_type, access_object, is_wildcard,
  grant_option, is_partial_revoke
FROM system.grants WHERE user_name = {user:String} ORDER BY toString(access_type) FORMAT TSV";
const TELEMETRY_QUERY_EXPECTED_GRANTS: [&str; 2] = [
    "ALTER USER\ttrustiot_tenant_account_\t1\t0\t0",
    "CREATE USER\ttrustiot_tenant_account_\t1\t0\t0",
];
/// telemetry-query's roles: the tenant role with ADMIN OPTION, not as a default role.
const TELEMETRY_QUERY_EXPECTED_ROLES: [&str; 1] = ["trustiot_tenant_reader\t0\t1"];
/// Whether a role exists.
const ROLE_EXISTS: &str = "SELECT count() FROM system.roles WHERE name = {role:String} FORMAT TSV";
const CREATE_INGEST: &str = "CREATE USER {user:Identifier}
IDENTIFIED WITH sha256_hash BY {hash:String} SALT {salt:String}
DEFAULT ROLE trustiot_ingest";
/// The ingest role's grants, one line each: privilege, database, table, column (empty for a whole
/// table), grant option, partial revoke. They must be exactly [`INGEST_EXPECTED_GRANTS`].
const INGEST_ROLE_GRANTS: &str = "SELECT access_type, ifNull(database, ''), ifNull(table, ''),
  ifNull(column, ''), toUInt8(grant_option), toUInt8(is_partial_revoke)
FROM system.grants WHERE role_name = {role:String}
ORDER BY ifNull(database, ''), ifNull(table, ''), toString(access_type) FORMAT TSV";
/// What the DDL files grant the ingest role after `0012_gateway_stream_ids.sql`: SELECT, INSERT
/// and ALTER DELETE (its lightweight DELETE) on the raw readings (`0004_access.sql`), SELECT and
/// INSERT on the completion records (`0012_gateway_stream_ids.sql`, which revoked the retired
/// `batch_commits`) and on the two aggregates (`0010_telemetry_rollups.sql`). A migration file
/// that changes the role's grants changes this list in the same change
/// (tests/clickhouse_ingest.rs runs every file against the pinned ClickHouse).
const INGEST_EXPECTED_GRANTS: [&str; 9] = [
    "INSERT\ttrustiot\tstream_commits\t\t0\t0",
    "SELECT\ttrustiot\tstream_commits\t\t0\t0",
    "ALTER DELETE\ttrustiot\ttelemetry\t\t0\t0",
    "INSERT\ttrustiot\ttelemetry\t\t0\t0",
    "SELECT\ttrustiot\ttelemetry\t\t0\t0",
    "INSERT\ttrustiot\ttelemetry_1h\t\t0\t0",
    "SELECT\ttrustiot\ttelemetry_1h\t\t0\t0",
    "INSERT\ttrustiot\ttelemetry_1m\t\t0\t0",
    "SELECT\ttrustiot\ttelemetry_1m\t\t0\t0",
];
/// Roles granted to the ingest role: none, or the account would hold more than the list above.
const ROLE_ROLES: &str =
    "SELECT count() FROM system.role_grants WHERE role_name = {role:String} FORMAT TSV";
/// Who holds the ingest role besides [`INGEST_ACCOUNT`] (whose grant the account check reads):
/// every holder writes and deletes every tenant's telemetry, and could stall a stream's writes
/// by running a long query with its query ID (services/ingest/README.md, 「I1 重寫與對帳」).
const INGEST_GRANTEES: &str = "SELECT if(user_name IS NULL, concat('role ', role_name), user_name)
FROM system.role_grants
WHERE granted_role_name = {role:String}
  AND NOT (user_name IS NOT NULL AND user_name = {user:String})
ORDER BY 1 FORMAT TSV";
/// How many host rules let the account log in; `HOST NONE` leaves none.
const USER_HOSTS: &str = "SELECT length(host_ip) + length(host_names) + length(host_names_regexp)
  + length(host_names_like) FROM system.users WHERE name = {user:String} FORMAT TSV";
/// The unit a managed TTL counts in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TtlUnit {
    /// The raw data and the completion records (Laysi/trustiot-v2#92).
    Days,
    /// The aggregates (Laysi/trustiot-v2#141).
    Months,
}

impl TtlUnit {
    /// The unit's name in the run's messages, and the name of the `MODIFY TTL` parameter.
    const fn name(self) -> &'static str {
        match self {
            Self::Days => "days",
            Self::Months => "months",
        }
    }
}

/// A table whose TTL is a retention the run sets: the server-time column it counts from, its
/// unit, and the statement that sets it.
struct RetentionTable {
    table: &'static str,
    column: &'static str,
    unit: TtlUnit,
    modify: &'static str,
}

impl RetentionTable {
    /// The TTL as the migration files create it, for the message that asks an administrator to
    /// set it back.
    fn form(&self) -> String {
        match self.unit {
            TtlUnit::Days => format!("TTL toDateTime({}) + toIntervalDay(<days>)", self.column),
            TtlUnit::Months => format!(
                "TTL {} + toIntervalMonth(<months>) + toIntervalDay(1)",
                self.column
            ),
        }
    }
}

/// Raw telemetry expires by the server's receive time, never the gateway's timestamp
/// (Laysi/trustiot#180).
const TELEMETRY_RETENTION: RetentionTable = RetentionTable {
    table: "telemetry",
    column: "received_at",
    unit: TtlUnit::Days,
    modify: "ALTER TABLE trustiot.telemetry
MODIFY TTL toDateTime(received_at) + toIntervalDay({days:UInt32})",
};

/// The completion records (the high-water marks) get the same number of days: `committed_at` is
/// never earlier than the `received_at` of the batch's rows, so a record outlives its rows.
///
/// The table keyed by gateway plus stream (`0012_gateway_stream_ids.sql`). The retired
/// `trustiot.batch_commits` is no longer managed: nothing writes or reads it (the ingest role
/// has no privilege on it), so its TTL protects nothing, and leaving it alone lets a later
/// cleanup remove it without breaking the run.
const COMMITS_RETENTION: RetentionTable = RetentionTable {
    table: "stream_commits",
    column: "committed_at",
    unit: TtlUnit::Days,
    modify: "ALTER TABLE trustiot.stream_commits
MODIFY TTL toDateTime(committed_at) + toIntervalDay({days:UInt32})",
};

/// The aggregates expire by the day the server received their readings, a whole number of
/// calendar months after it; the day added keeps a cell until its last reading of that day is
/// at least the months old (0010_telemetry_rollups.sql).
const MINUTE_RETENTION: RetentionTable = RetentionTable {
    table: "telemetry_1m",
    column: "received_day",
    unit: TtlUnit::Months,
    modify: "ALTER TABLE trustiot.telemetry_1m
MODIFY TTL received_day + toIntervalMonth({months:UInt32}) + toIntervalDay(1)",
};

const HOUR_RETENTION: RetentionTable = RetentionTable {
    table: "telemetry_1h",
    column: "received_day",
    unit: TtlUnit::Months,
    modify: "ALTER TABLE trustiot.telemetry_1h
MODIFY TTL received_day + toIntervalMonth({months:UInt32}) + toIntervalDay(1)",
};

/// The tables whose TTL the run sets, from the one that must keep its rows longest to the one
/// that may keep them shortest: each keeps them at least as long as the next
/// (`retention::AggregateRetention::new` checks the settings; the run keeps it true while it
/// changes the tables one after the other).
const RETENTION_TABLES: [&RetentionTable; 4] = [
    &HOUR_RETENTION,
    &MINUTE_RETENTION,
    &COMMITS_RETENTION,
    &TELEMETRY_RETENTION,
];

/// The settings in the order of [`RETENTION_TABLES`].
const fn retention_targets(retention: Retention) -> [u32; 4] {
    [
        retention.aggregates.hour_months(),
        retention.aggregates.minute_months(),
        retention.raw.days(),
        retention.raw.days(),
    ]
}

/// The TTL changes that take the tables of [`RETENTION_TABLES`] from `current` to `targets`, in
/// the order the run makes them, as (index, from, to): first every table that gets longer, from
/// the longest-lived one down, then every table that gets shorter, from the shortest-lived one
/// up. When `current` and `targets` both keep each table at least as long as the next, so does
/// every state in between: a table grows only once the tables before it reached their target
/// (at least its own) or already kept their rows longer, and shrinks only once the tables after
/// it reached theirs (at most its own).
fn retention_steps(
    current: [u32; RETENTION_TABLES.len()],
    targets: [u32; RETENTION_TABLES.len()],
) -> Vec<(usize, u32, u32)> {
    let changes = || current.into_iter().zip(targets).enumerate();
    let longer = changes().filter(|(_, (from, to))| from < to);
    let shorter = changes().rev().filter(|(_, (from, to))| from > to);
    longer
        .chain(shorter)
        .map(|(index, (from, to))| (index, from, to))
        .collect()
}

const TABLE_ENGINE: &str = "SELECT engine_full FROM system.tables
WHERE database = 'trustiot' AND name = {table:String} FORMAT JSONEachRow";

/// The TTL clause of a table's `engine_full`, between ` TTL ` and ` SETTINGS `.
fn ttl_clause(engine_full: &str) -> Result<&str, String> {
    let (_, after) = engine_full
        .split_once(" TTL ")
        .ok_or_else(|| "it has no TTL".to_owned())?;
    Ok(after.split_once(" SETTINGS ").map_or(after, |(ttl, _)| ttl))
}

/// A whole number of the TTL: digits, or `_CAST(<digits>, 'UInt32')`, which is how ClickHouse
/// 26.8 stores the query parameter of the run's own `MODIFY TTL`.
fn ttl_number(text: &str) -> Option<u32> {
    let digits = text
        .strip_prefix("_CAST(")
        .and_then(|rest| rest.strip_suffix(", 'UInt32')"))
        .unwrap_or(text);
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// The days of a TTL the run manages, read from a table's `engine_full`: exactly
/// `TTL toDateTime(<column>) + toIntervalDay(<days>)`, as the migration files create it, or with
/// `_CAST(<days>, 'UInt32')` as the days, which is how ClickHouse 26.8 stores the query parameter
/// of the run's own `MODIFY TTL`.
///
/// # Errors
///
/// When the table has no TTL or another one (an administrator changed it, for example to move
/// old parts to another volume): the run does not overwrite what it does not recognise.
pub fn managed_ttl_days(engine_full: &str, column: &str) -> Result<u32, String> {
    let ttl = ttl_clause(engine_full)?;
    ttl.strip_prefix("toDateTime(")
        .and_then(|rest| rest.strip_prefix(column))
        .and_then(|rest| rest.strip_prefix(") + toIntervalDay("))
        .and_then(|rest| rest.strip_suffix(')'))
        .and_then(ttl_number)
        .ok_or_else(|| format!("its TTL `{ttl}` is not one the migration Job manages"))
}

/// The months of an aggregate table's TTL the run manages, read from its `engine_full`: exactly
/// `TTL <column> + toIntervalMonth(<months>) + toIntervalDay(1)`, which ClickHouse 26.8 stores as
/// `(<column> + toIntervalMonth(<months>)) + toIntervalDay(1)`, with the months as digits (the
/// migration file) or `_CAST(<months>, 'UInt32')` (the run's own `MODIFY TTL`).
///
/// # Errors
///
/// As [`managed_ttl_days`].
pub fn managed_ttl_months(engine_full: &str, column: &str) -> Result<u32, String> {
    let ttl = ttl_clause(engine_full)?;
    ttl.strip_prefix('(')
        .and_then(|rest| rest.strip_prefix(column))
        .and_then(|rest| rest.strip_prefix(" + toIntervalMonth("))
        .and_then(|rest| rest.strip_suffix(")) + toIntervalDay(1)"))
        .and_then(ttl_number)
        .ok_or_else(|| format!("its TTL `{ttl}` is not one the migration Job manages"))
}

/// Revokes an account's login without dropping it: reversible with `HOST ANY`, which is what
/// `CREATE USER` gives it.
const LOGIN_NONE: &str = "ALTER USER {user:Identifier} HOST NONE";
const LOGIN_ANY: &str = "ALTER USER {user:Identifier} HOST ANY";

/// The access-management account's client.
pub struct ClickHouse {
    client: http::Client,
    url: String,
    username: String,
    password: String,
}

/// Splits a migration file into statements, by the file format of services/ingest/README.md:
/// lines whose first non-blank characters are `--` are comments; a statement ends with `;` at
/// the end of a line, and `;` appears nowhere else.
///
/// # Errors
///
/// When the file breaks that format.
pub fn split_statements(text: &str) -> Result<Vec<String>, String> {
    let mut statements = Vec::new();
    let mut current = String::new();
    for (index, line) in text.lines().enumerate() {
        let trimmed = line.trim_end();
        if trimmed.trim_start().starts_with("--") {
            continue;
        }
        let (body, ends) = match trimmed.strip_suffix(';') {
            Some(body) => (body, true),
            None => (trimmed, false),
        };
        if body.contains(';') {
            return Err(format!(
                "line {}: `;` may only end a statement, at the end of a line",
                index + 1
            ));
        }
        if !body.trim().is_empty() {
            current.push_str(body);
            current.push('\n');
        }
        if ends {
            let statement = current.trim().to_owned();
            if statement.is_empty() {
                return Err(format!("line {}: empty statement", index + 1));
            }
            statements.push(statement);
            current.clear();
        }
    }
    if !current.trim().is_empty() {
        return Err("the last statement does not end with `;`".to_owned());
    }
    Ok(statements)
}

fn cut(text: &str) -> String {
    text.chars().take(ERROR_TEXT_LIMIT).collect()
}

/// A request that did not get a 200: the status and ClickHouse's error text.
struct Failure {
    status: Option<u16>,
    text: String,
}

impl Failure {
    fn is_authentication(&self) -> bool {
        self.text.contains("AUTHENTICATION_FAILED") || self.text.contains("Authentication failed")
    }
}

/// How ClickHouse answered the access-management account's login.
enum Answer {
    Accepted,
    /// It refused the password; ClickHouse's error text, which never holds the password.
    Refused(String),
}

impl ClickHouse {
    /// A client for `TRUSTIOT_CLICKHOUSE_URL`; an `https://` URL needs
    /// `TRUSTIOT_CLICKHOUSE_CA_FILE`, the only CA it trusts.
    ///
    /// # Errors
    ///
    /// When the URL is https without a CA file, or the client cannot be built.
    fn new(config: &RunConfig, username: &str, password: String) -> Result<Self, Error> {
        let url =
            config.platform.clickhouse_url.clone().ok_or_else(|| {
                Error::ClickHouse("TRUSTIOT_CLICKHOUSE_URL is not set".to_owned())
            })?;
        let client = if url.starts_with("https://") {
            let ca = config
                .platform
                .clickhouse_ca_file
                .as_deref()
                .ok_or_else(|| {
                    Error::ClickHouse(
                    "an https ClickHouse needs TRUSTIOT_CLICKHOUSE_CA_FILE: the Job trusts only \
                     the CA the deployment names (clickhouse.external.caSecret)"
                        .to_owned(),
                )
                })?;
            http::trusting(ca).map_err(|error| Error::ClickHouse(error.to_string()))?
        } else {
            http::plain()
        };
        Ok(Self {
            client,
            url: url.trim_end_matches('/').to_owned(),
            username: username.to_owned(),
            password,
        })
    }

    /// A client for the access-management account of `settings`, once ClickHouse answers a query
    /// as it, waiting up to `wait` (on a fresh install ClickHouse starts next to the Job).
    ///
    /// With `password_secret`, the password comes from that Secret, which the bundled
    /// ClickHouse's init container fills before the server starts (src/clickhouse_admin.rs): the
    /// run waits until it is filled, and while ClickHouse refuses the password it read, it reads
    /// the Secret again. That is a rotation in progress: the init container writes the new
    /// password into the Secret just before the server restarts with it, so the server can refuse
    /// a password read a moment earlier, or still run with the old one for a moment after.
    ///
    /// # Errors
    ///
    /// When ClickHouse did not answer, or did not accept the password, within `wait`; when the
    /// Secret is missing, unreadable or not filled in time.
    pub async fn connect(
        config: &RunConfig,
        settings: &ClickHouseSettings,
        wait: Duration,
    ) -> Result<Self, Error> {
        let deadline = Instant::now() + wait;
        let (secret, passwords) = match (
            &settings.password,
            &settings.password_secret,
            &config.passwords,
        ) {
            (Some(password), None, _) => {
                let client = Self::new(config, &settings.username, password.clone())?;
                client.wait_for_answer(deadline, false).await?;
                return Ok(client);
            }
            (None, Some(secret), Some(passwords)) => (secret, passwords),
            _ => {
                return Err(Error::ClickHouse(
                    "internal: the settings name no usable source for the ClickHouse password"
                        .to_owned(),
                ));
            }
        };
        let api = Api::new(passwords.into(), wait, clickhouse_admin::JOB_HINTS)?;
        let account = settings.username.as_str();
        let mut client = Self::new(config, account, String::new())?;
        let mut reported = None::<Instant>;
        loop {
            let password =
                clickhouse_admin::wait_for_password(&api, secret, account, deadline).await?;
            password.expose().clone_into(&mut client.password);
            match client.wait_for_answer(deadline, true).await? {
                Answer::Accepted => return Ok(client),
                Answer::Refused(text) => {
                    if Instant::now() >= deadline {
                        return Err(Error::ClickHouse(format!(
                            "did not accept {account} with the password in Secret {secret} within \
                             {wait:?}: {text}. The server reads the password only when it starts, \
                             and its init container writes it into the Secret first; restart \
                             ClickHouse if the Secret was replaced (docs/runbooks/secrets.md)"
                        )));
                    }
                    if reported.is_none_or(|at: Instant| at.elapsed() >= REPORT_EVERY) {
                        tracing::warn!(
                            secret = %secret,
                            account,
                            "ClickHouse refused the admin password in the Secret (a rotation in \
                             progress?); reading it again"
                        );
                        reported = Some(Instant::now());
                    }
                    tokio::time::sleep(RETRY_EVERY).await;
                }
            }
        }
    }

    /// Sends `query` with `params` as query parameters (`{name:Type}`) and `settings` as
    /// ClickHouse settings, both in the URL.
    async fn send(
        &self,
        username: &str,
        password: &str,
        query: &str,
        params: &[(&str, &str)],
        settings: &[(&str, &str)],
    ) -> Result<String, Failure> {
        let mut pairs: Vec<(String, &str)> = params
            .iter()
            .map(|(name, value)| (format!("param_{name}"), *value))
            .collect();
        pairs.extend(
            settings
                .iter()
                .map(|(name, value)| ((*name).to_owned(), *value)),
        );
        pairs.push(("wait_end_of_query".to_owned(), "1"));
        let pairs: Vec<(&str, &str)> = pairs
            .iter()
            .map(|(name, value)| (name.as_str(), *value))
            .collect();
        let response = self
            .client
            .post(
                &format!("{}/", self.url),
                &pairs,
                http::Auth::Basic { username, password },
                query.to_owned(),
            )
            .await
            .map_err(|error| Failure {
                status: None,
                text: error.to_string(),
            })?;
        if response.status.is_success() {
            Ok(response.body)
        } else {
            Err(Failure {
                status: Some(response.status.as_u16()),
                text: cut(&response.body),
            })
        }
    }

    /// Runs `query` as the access-management account.
    async fn query(
        &self,
        what: &str,
        query: &str,
        params: &[(&str, &str)],
    ) -> Result<String, Error> {
        self.query_with_settings(what, query, params, &[]).await
    }

    /// Runs `query` as the access-management account with ClickHouse `settings`.
    async fn query_with_settings(
        &self,
        what: &str,
        query: &str,
        params: &[(&str, &str)],
        settings: &[(&str, &str)],
    ) -> Result<String, Error> {
        self.send(&self.username, &self.password, query, params, settings)
            .await
            .map_err(|failure| {
                Error::ClickHouse(format!(
                    "{what}: {}{}",
                    failure
                        .status
                        .map(|status| format!("HTTP {status} "))
                        .unwrap_or_default(),
                    failure.text.trim()
                ))
            })
    }

    /// Waits until ClickHouse answers a query as the access-management account, until
    /// `deadline`. With `refusal_ends_wait`, a refused login ends the wait at once (the caller
    /// reads the password again); otherwise it is retried like any other failure.
    ///
    /// # Errors
    ///
    /// When it did not answer by `deadline`.
    async fn wait_for_answer(
        &self,
        deadline: Instant,
        refusal_ends_wait: bool,
    ) -> Result<Answer, Error> {
        let started = Instant::now();
        let mut reported = None::<Instant>;
        loop {
            let failure = match self
                .send(&self.username, &self.password, PROBE, &[], &[])
                .await
            {
                Ok(_) => return Ok(Answer::Accepted),
                Err(failure) => failure,
            };
            let error = format!(
                "{}{}",
                failure
                    .status
                    .map(|status| format!("HTTP {status} "))
                    .unwrap_or_default(),
                failure.text.trim()
            );
            if refusal_ends_wait && failure.is_authentication() {
                return Ok(Answer::Refused(error));
            }
            if Instant::now() >= deadline {
                return Err(Error::ClickHouse(format!(
                    "not reachable within {:?}: ping: {error}",
                    started.elapsed()
                )));
            }
            if reported.is_none_or(|at| at.elapsed() >= REPORT_EVERY) {
                tracing::warn!(error = %error, "ClickHouse does not answer yet; retrying");
                reported = Some(Instant::now());
            }
            tokio::time::sleep(RETRY_EVERY).await;
        }
    }

    /// The DDL files, the raw and aggregate data retention, the backfill of the aggregates and
    /// the check that the backfill views show nothing (when the registry has ClickHouse files,
    /// which create the tables they apply to), then the accounts of the collector and of
    /// telemetry-query and of ingest.
    ///
    /// # Errors
    ///
    /// The first statement that fails, a file that changed after it was applied, a TTL the run
    /// does not recognise, an account that holds more than the Job gives it, or a backfill
    /// view's policy that still names an account after the run set it to name nobody.
    pub async fn migrate(
        &self,
        registry: &Registry,
        retention: Retention,
        collector: Option<&(String, Password)>,
        telemetry_query: Option<&Password>,
        ingest: Option<&Password>,
        report: &mut Report,
    ) -> Result<(), Error> {
        for statement in BOOTSTRAP {
            self.query(
                "create the bookkeeping table trustiot_migrate.applied",
                statement,
                &[],
            )
            .await?;
        }
        for migration in registry.clickhouse {
            self.apply(migration, report).await?;
        }
        if !registry.clickhouse.is_empty() {
            let raw_before = self.apply_retention(retention, report).await?;
            // A raw retention this run lengthened has not kept the rows the shorter one already
            // deleted: the backfill compares only the days both kept.
            self.backfill_aggregates(raw_before.min(retention.raw.days()), report)
                .await?;
            self.hide_backfill_views(report).await?;
        }
        if let Some((user, password)) = collector {
            self.ensure_collector(user, password, report).await?;
        }
        self.retire_collectors(collector.map(|(user, _)| user.as_str()), report)
            .await?;
        self.ensure_telemetry_query(telemetry_query, report).await?;
        self.ensure_ingest(ingest, report).await
    }

    /// Whether `user` may log in from any host at all.
    async fn can_log_in(&self, user: &str) -> Result<bool, Error> {
        let hosts = self
            .query(
                &format!("read the hosts of {user}"),
                USER_HOSTS,
                &[("user", user)],
            )
            .await?;
        Ok(hosts.trim() != "0")
    }

    /// Revokes the login of every collector account but `keep` (the collector is off, or its
    /// account was renamed): nothing uses it any more, and its password stays in a kept Secret.
    /// Never drops it; the next run that needs it lets it log in again.
    async fn retire_collectors(
        &self,
        keep: Option<&str>,
        report: &mut Report,
    ) -> Result<(), Error> {
        let accounts = self
            .query(
                "list the collector accounts",
                COLLECTOR_ACCOUNTS,
                &[("role", COLLECTOR_ROLE)],
            )
            .await?;
        for user in accounts.lines().filter(|line| !line.is_empty()) {
            if Some(user) == keep || !self.can_log_in(user).await? {
                continue;
            }
            self.query(
                &format!("revoke the login of {user}"),
                LOGIN_NONE,
                &[("user", user)],
            )
            .await?;
            report.change(format!(
                "revoked the login of ClickHouse account {user}: the collector no longer uses it"
            ));
        }
        Ok(())
    }

    async fn apply(&self, migration: &Migration, report: &mut Report) -> Result<(), Error> {
        let label = format!("clickhouse/{}", migration.name);
        let sha256 = sha256_hex(migration.sql);
        let recorded = self
            .query(
                &format!("read whether {label} was applied"),
                APPLIED,
                &[("name", migration.name)],
            )
            .await?;
        let recorded: Vec<&str> = recorded.lines().filter(|line| !line.is_empty()).collect();
        if recorded.iter().any(|hash| *hash != sha256) {
            return Err(Error::Changed {
                component: "clickhouse".to_owned(),
                name: migration.name.to_owned(),
            });
        }
        if !recorded.is_empty() {
            tracing::debug!(migration = %label, "already applied");
            return Ok(());
        }
        let statements = split_statements(migration.sql)
            .map_err(|problem| Error::ClickHouse(format!("{label}: {problem}")))?;
        for (index, statement) in statements.iter().enumerate() {
            self.query(&format!("{label}, statement {}", index + 1), statement, &[])
                .await?;
        }
        self.query(
            &format!("record {label}"),
            RECORD,
            &[("name", migration.name), ("sha256", &sha256)],
        )
        .await?;
        report.change(format!("applied {label}"));
        Ok(())
    }

    /// Sets the TTL of the telemetry, completion and aggregate tables to `retention`, where it
    /// differs.
    ///
    /// Each table of [`RETENTION_TABLES`] must keep its rows at least as long as the next: the
    /// completion records never expire before the rows they describe, the one-hour aggregates
    /// never before the one-minute ones, and those never before the raw rows they summarise.
    /// That stays true while the run changes the tables one after the other: it first lengthens
    /// the tables that get longer, from the longest-lived one down, then shortens the ones that
    /// get shorter, from the shortest-lived one up. It reads every TTL before it changes any, so
    /// a TTL it does not recognise stops the run before anything changed. ClickHouse applies a
    /// new TTL to the existing parts with a `MATERIALIZE TTL` mutation that runs in the
    /// background (`materialize_ttl_after_modify`, on by default): without it, parts written
    /// under the old TTL would still expire by the old one.
    ///
    /// Returns the raw data retention, in days, that the telemetry table had before the run.
    async fn apply_retention(
        &self,
        retention: Retention,
        report: &mut Report,
    ) -> Result<u32, Error> {
        let mut current = [0; RETENTION_TABLES.len()];
        for (now, table) in current.iter_mut().zip(RETENTION_TABLES) {
            *now = self.ttl(table).await?;
        }
        for (index, from, to) in retention_steps(current, retention_targets(retention)) {
            self.set_ttl(RETENTION_TABLES[index], from, to, report)
                .await?;
        }
        Ok(current[RETENTION_TABLES.len() - 1])
    }

    /// Fills the one-minute and one-hour aggregates from the raw rows the telemetry table held
    /// before their materialized views existed, once: when `trustiot_migrate.applied` has no
    /// [`BACKFILL_RECORD`]. It runs after the TTLs are set, so a backfilled cell expires by the
    /// aggregate retention of the deployment and not by the files' default (a cell the
    /// one-minute table's 13 months would drop is kept when the setting is longer).
    ///
    /// The statements insert the cells the aggregates lack, recomputed from the raw rows, one
    /// receive day at a time ([`BACKFILL_1M`], [`BACKFILL_1H`]), over the days
    /// [`backfill_day_count`] gives: none before today - `raw_days` + 3, inside the views' bound,
    /// so no cell is recomputed from raw rows of which some may have expired. They never change a cell the aggregates hold, so
    /// a run after a failed one fills what is still missing, and a run after a lost record (a
    /// ClickHouse restored without `trustiot_migrate`) takes nothing away, even where the raw
    /// rows are gone. The views show their rows to the run's account only while it backfills,
    /// whichever account the run uses.
    async fn backfill_aggregates(&self, raw_days: u32, report: &mut Report) -> Result<(), Error> {
        let recorded = self
            .query(
                "read whether the aggregates were backfilled",
                RECORDED,
                &[("name", BACKFILL_RECORD)],
            )
            .await?;
        if recorded.trim() != "0" {
            return Ok(());
        }
        self.query(
            "show the backfill views to the run's account",
            SHOW_BACKFILL_VIEWS,
            &[],
        )
        .await?;
        let filled = self.fill_aggregates(raw_days).await;
        // Hidden again whether or not the backfill went through: a failed run is repeated.
        let hidden = self
            .query("hide the backfill views again", HIDE_BACKFILL_VIEWS, &[])
            .await;
        let (first, last) = filled?;
        hidden?;
        let sha256 = sha256_hex(&format!("{BACKFILL_1M}\n{BACKFILL_1H}"));
        self.query(
            "record the backfill of the aggregates",
            RECORD,
            &[("name", BACKFILL_RECORD), ("sha256", &sha256)],
        )
        .await?;
        report.change(format!(
            "backfilled the one-minute and one-hour aggregates from the raw rows received from \
             {first} to {last} (raw data retention {raw_days} days, less the three oldest days)"
        ));
        Ok(())
    }

    /// Makes the backfill views show their rows to nobody, on every run, whether or not the
    /// aggregates were backfilled before: a run stopped between showing and hiding them (a Job
    /// killed at its `activeDeadlineSeconds`, a lost connection) or an administrator's
    /// `ALTER ROW POLICY` leaves a `migration_account` policy naming an account, and nothing
    /// else names nobody again once the backfill is recorded. A policy that names an account, a
    /// role or everyone is set to `TO NONE`, reported as a change, and read again.
    ///
    /// # Errors
    ///
    /// When a policy still names an account after that.
    async fn hide_backfill_views(&self, report: &mut Report) -> Result<(), Error> {
        let bound = self
            .query(
                "read the backfill views' row policies",
                BOUND_BACKFILL_POLICIES,
                &[],
            )
            .await?;
        if bound.trim().is_empty() {
            return Ok(());
        }
        self.query(
            "set the backfill views' row policies to name nobody",
            HIDE_BACKFILL_VIEWS,
            &[],
        )
        .await?;
        let still = self
            .query(
                "read the backfill views' row policies again",
                BOUND_BACKFILL_POLICIES,
                &[],
            )
            .await?;
        if !still.trim().is_empty() {
            return Err(Error::ClickHouse(format!(
                "the migration_account row policies of the backfill views still name {} after \
                 the run set them TO NONE; an administrator must set them TO NONE \
                 (docs/runbooks/migrations.md)",
                bound_policies(&still)
            )));
        }
        report.change(format!(
            "set the migration_account row policies of {} back to name nobody",
            bound_policies(&bound)
        ));
        Ok(())
    }

    /// Runs the two backfill statements for each receive day [`backfill_day_count`] gives,
    /// oldest first; the last step reaches to [`LAST_DAY`]. The first and the last day.
    async fn fill_aggregates(&self, raw_days: u32) -> Result<(String, String), Error> {
        let raw = raw_days.to_string();
        let count = backfill_day_count(raw_days).to_string();
        let listed = self
            .query(
                "list the receive days to backfill",
                BACKFILL_DAYS,
                &[("count", &count)],
            )
            .await?;
        let days: Vec<&str> = listed.lines().filter(|line| !line.is_empty()).collect();
        let (Some(first), Some(last)) = (days.first(), days.last()) else {
            return Err(Error::ClickHouse(format!(
                "list the receive days to backfill: no day for a raw data retention of \
                 {raw_days} days"
            )));
        };
        let max_seconds = BACKFILL_MAX_SECONDS.to_string();
        let settings = [("max_execution_time", max_seconds.as_str())];
        for (index, day) in days.iter().enumerate() {
            let until = if index + 1 == days.len() {
                LAST_DAY
            } else {
                day
            };
            let params = [
                ("first_day", *day),
                ("last_day", until),
                ("raw_retention_days", raw.as_str()),
            ];
            for (table, statement) in [("telemetry_1m", BACKFILL_1M), ("telemetry_1h", BACKFILL_1H)]
            {
                self.query_with_settings(
                    &format!("backfill trustiot.{table} for the receive day {day}"),
                    statement,
                    &params,
                    &settings,
                )
                .await?;
            }
        }
        Ok(((*first).to_owned(), (*last).to_owned()))
    }

    /// The retention that `table` has now, in its unit.
    async fn ttl(&self, table: &RetentionTable) -> Result<u32, Error> {
        let what = format!("read the TTL of trustiot.{}", table.table);
        let row = self
            .query(&what, TABLE_ENGINE, &[("table", table.table)])
            .await?;
        let engine_full = serde_json::from_str::<serde_json::Value>(row.trim())
            .ok()
            .and_then(|row| row["engine_full"].as_str().map(str::to_owned))
            .ok_or_else(|| {
                Error::ClickHouse(format!(
                    "{what}: the table is missing (answer: {})",
                    cut(&row)
                ))
            })?;
        let managed = match table.unit {
            TtlUnit::Days => managed_ttl_days(&engine_full, table.column),
            TtlUnit::Months => managed_ttl_months(&engine_full, table.column),
        };
        managed.map_err(|problem| {
            Error::ClickHouse(format!(
                "trustiot.{}: {problem}; an administrator must set it back to `{}` \
                 (docs/runbooks/migrations.md)",
                table.table,
                table.form()
            ))
        })
    }

    async fn set_ttl(
        &self,
        table: &RetentionTable,
        from: u32,
        to: u32,
        report: &mut Report,
    ) -> Result<(), Error> {
        let unit = table.unit.name();
        let value = to.to_string();
        self.query(
            &format!("set the TTL of trustiot.{} to {to} {unit}", table.table),
            table.modify,
            &[(unit, &value)],
        )
        .await?;
        report.change(format!(
            "set the retention of ClickHouse table trustiot.{} to {to} {unit} (was {from})",
            table.table
        ));
        Ok(())
    }

    /// The collector's account: exactly the collector role, no other grants, and a password
    /// that logs in. An account that has anything else stops the run.
    async fn ensure_collector(
        &self,
        user: &str,
        password: &Password,
        report: &mut Report,
    ) -> Result<(), Error> {
        let params = [("user", user)];
        let extras = self
            .query(
                "read the collector role's grants",
                ROLE_EXTRAS,
                &[("role", COLLECTOR_ROLE)],
            )
            .await?;
        if extras.trim() != "0" {
            return Err(Error::ClickHouse(format!(
                "the role {COLLECTOR_ROLE} has grants or roles beyond 0006_logs.sql; an \
                 administrator must revoke them (docs/runbooks/migrations.md)"
            )));
        }
        let exists = self
            .query("look for the collector account", USER_EXISTS, &params)
            .await?;
        let create = exists.trim() == "0";
        if !create {
            let roles = self
                .query("read the collector account's roles", USER_ROLES, &params)
                .await?;
            let grants = self
                .query("read the collector account's grants", USER_GRANTS, &params)
                .await?;
            let settings = self
                .query(
                    "read the collector account's settings",
                    USER_SETTINGS,
                    &params,
                )
                .await?;
            let roles: Vec<&str> = roles.lines().filter(|line| !line.is_empty()).collect();
            let expected = format!("{COLLECTOR_ROLE}\t1\t0");
            if roles != [expected.as_str()] || grants.trim() != "0" || settings.trim() != "0" {
                // Never repaired by the Job: someone changed the account by hand, and dropping
                // it is an administrator's decision (DROP USER takes no query parameters either).
                return Err(Error::ClickHouse(format!(
                    "the collector account {user} has other roles, grants or settings than \
                     {COLLECTOR_ROLE} as its default role; an administrator must drop it \
                     (DROP USER {user}), and the next run creates it again \
                     (docs/runbooks/migrations.md)"
                )));
            }
        }
        let (hash, salt) = password
            .clickhouse_sha256()
            .map_err(|error| Error::Random(error.to_string()))?;
        let auth = [
            ("user", user),
            ("hash", hash.as_str()),
            ("salt", salt.as_str()),
        ];
        if create {
            self.query("create the collector account", CREATE_COLLECTOR, &auth)
                .await?;
            report.change(format!("created ClickHouse account {user}"));
            return Ok(());
        }
        if !self.can_log_in(user).await? {
            // A run that did not need the account revoked its login; it is needed again. The
            // Secret's password first, so the account never logs in again with an older one.
            self.query("set the collector account's password", SET_PASSWORD, &auth)
                .await?;
            report.change(format!("set the password of ClickHouse account {user}"));
            self.query(
                &format!("let {user} log in again"),
                LOGIN_ANY,
                &[("user", user)],
            )
            .await?;
            report.change(format!("let ClickHouse account {user} log in again"));
            return Ok(());
        }
        match self.send(user, password.expose(), PROBE, &[], &[]).await {
            Ok(_) => Ok(()),
            Err(failure) if failure.is_authentication() => {
                self.query("set the collector account's password", SET_PASSWORD, &auth)
                    .await?;
                report.change(format!("set the password of ClickHouse account {user}"));
                Ok(())
            }
            Err(failure) => Err(Error::ClickHouse(format!(
                "log in as {user} to check its password: {}",
                failure.text.trim()
            ))),
        }
    }

    /// telemetry-query's account (`0008_telemetry_query.sql`): when the run manages its password
    /// (the service is deployed) the account logs in with it from any host; when not, it cannot
    /// log in at all (`HOST NONE`), so the password a kept Secret still holds creates no tenant
    /// account. Either way the account, and the tenant role it hands to every tenant account,
    /// must hold exactly what the DDL gave them; anything else revokes the account's login and
    /// stops the run for an administrator, because this account creates and changes every
    /// tenant's account. Once an administrator has repaired it, the next run lets it log in
    /// again.
    async fn ensure_telemetry_query(
        &self,
        password: Option<&Password>,
        report: &mut Report,
    ) -> Result<(), Error> {
        let user = TELEMETRY_QUERY_ACCOUNT;
        let params = [("user", user)];
        let exists = self
            .query("look for the telemetry-query account", USER_EXISTS, &params)
            .await?;
        if exists.trim() == "0" {
            // Only a registry without 0008_telemetry_query.sql gets here.
            return match password {
                None => Ok(()),
                Some(_) => Err(Error::ClickHouse(format!(
                    "the ClickHouse account {user} does not exist after the DDL files; \
                     0008_telemetry_query.sql creates it"
                ))),
            };
        }
        if let Some(finding) = self.audit_telemetry_query().await? {
            // Detection alone would leave the account able to act until an administrator reads
            // the log, and it can create and change every tenant's account.
            let revoked = self
                .query(&format!("revoke the login of {user}"), LOGIN_NONE, &params)
                .await;
            return Err(Error::ClickHouse(match revoked {
                Ok(_) => format!(
                    "{finding}; the run revoked the login of {user} (HOST NONE), and the first \
                     run after the repair lets it log in again while telemetry-query is deployed"
                ),
                Err(error) => {
                    format!("{finding}; revoking the login of {user} failed as well: {error}")
                }
            }));
        }
        let can_log_in = self.can_log_in(user).await?;
        let Some(password) = password else {
            if can_log_in {
                self.query(&format!("revoke the login of {user}"), LOGIN_NONE, &params)
                    .await?;
                report.change(format!(
                    "revoked the login of ClickHouse account {user}: telemetry-query is not \
                     deployed"
                ));
            }
            return Ok(());
        };
        let (hash, salt) = password
            .clickhouse_sha256()
            .map_err(|error| Error::Random(error.to_string()))?;
        let auth = [
            ("user", user),
            ("hash", hash.as_str()),
            ("salt", salt.as_str()),
        ];
        if !can_log_in {
            // Created by the DDL without a password, or its login revoked by a run without the
            // service: the Secret's password first, so it never logs in with another one.
            self.query(
                "set the telemetry-query account's password",
                SET_PASSWORD,
                &auth,
            )
            .await?;
            report.change(format!("set the password of ClickHouse account {user}"));
            self.query(&format!("let {user} log in"), LOGIN_ANY, &params)
                .await?;
            report.change(format!("let ClickHouse account {user} log in"));
            return Ok(());
        }
        match self.send(user, password.expose(), PROBE, &[], &[]).await {
            Ok(_) => Ok(()),
            Err(failure) if failure.is_authentication() => {
                self.query(
                    "set the telemetry-query account's password",
                    SET_PASSWORD,
                    &auth,
                )
                .await?;
                report.change(format!("set the password of ClickHouse account {user}"));
                Ok(())
            }
            Err(failure) => Err(Error::ClickHouse(format!(
                "log in as {user} to check its password: {}",
                failure.text.trim()
            ))),
        }
    }

    /// What about telemetry-query's account or the tenant role differs from what the DDL gave
    /// them, if anything.
    async fn audit_telemetry_query(&self) -> Result<Option<String>, Error> {
        let user = TELEMETRY_QUERY_ACCOUNT;
        let params = [("user", user)];
        let grantees_none = self
            .query(
                "read whom the telemetry-query account may grant to",
                TELEMETRY_QUERY_GRANTEES_NONE,
                &params,
            )
            .await?;
        if grantees_none.trim() != "1" {
            return Ok(Some(format!(
                "the ClickHouse account {user} may grant the role {TENANT_READER_ROLE} to other \
                 users or roles (its GRANTEES is not NONE, which 0009_telemetry_query_grantees.sql sets); \
                 an administrator must set GRANTEES NONE again and check who holds the role \
                 (docs/runbooks/migrations.md)"
            )));
        }
        let views = format!("['{}']", TENANT_VIEWS.join("','"));
        let reader = self
            .query(
                "read the tenant role's grants",
                READER_GRANTS,
                &[("role", TENANT_READER_ROLE), ("views", &views)],
            )
            .await?;
        let (extras, granted) = reader
            .trim()
            .split_once('\t')
            .unwrap_or((reader.trim(), ""));
        // Extras first: a SELECT with GRANT OPTION is an extra, not a missing grant.
        if extras != "0" {
            return Ok(Some(format!(
                "the role {TENANT_READER_ROLE} has grants or roles beyond SELECT on the tenant \
                 views ({}); every tenant account gets this role, so an administrator must \
                 revoke them (docs/runbooks/migrations.md)",
                TENANT_VIEWS
                    .iter()
                    .map(|view| format!("trustiot.{view}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
        let missing: Vec<String> = TENANT_VIEWS
            .iter()
            .filter(|view| !granted.split(',').any(|table| table == **view))
            .map(|view| format!("trustiot.{view}"))
            .collect();
        if !missing.is_empty() {
            return Ok(Some(format!(
                "the role {TENANT_READER_ROLE} lacks SELECT on {} (0005_tenant_view.sql and \
                 0010_telemetry_rollups.sql give it); no tenant account can read there until an \
                 administrator grants it again (docs/runbooks/migrations.md)",
                missing.join(", ")
            )));
        }
        let grantees = self
            .query(
                "read who holds the tenant role",
                READER_GRANTEES,
                &[
                    ("role", TENANT_READER_ROLE),
                    ("service", user),
                    ("prefix", TENANT_ACCOUNT_PREFIX),
                ],
            )
            .await?;
        let grantees: Vec<&str> = grantees.lines().filter(|line| !line.is_empty()).collect();
        if !grantees.is_empty() {
            let shown = grantees
                .iter()
                .take(GRANTEES_SHOWN)
                .copied()
                .collect::<Vec<_>>()
                .join(", ");
            return Ok(Some(format!(
                "the role {TENANT_READER_ROLE} is held by {} grantee(s) besides {user} (with ADMIN \
                 OPTION) and {TENANT_ACCOUNT_PREFIX}* accounts (without it): {shown}; every \
                 holder reads the tenant view, so an administrator must revoke it \
                 (docs/runbooks/migrations.md)",
                grantees.len()
            )));
        }
        let roles = self
            .query(
                "read the telemetry-query account's roles",
                USER_ROLES,
                &params,
            )
            .await?;
        let grants = self
            .query(
                "read the telemetry-query account's grants",
                TELEMETRY_QUERY_GRANTS,
                &params,
            )
            .await?;
        let settings = self
            .query(
                "read the telemetry-query account's settings",
                USER_SETTINGS,
                &params,
            )
            .await?;
        let roles: Vec<&str> = roles.lines().filter(|line| !line.is_empty()).collect();
        let grants: Vec<&str> = grants.lines().filter(|line| !line.is_empty()).collect();
        if roles != TELEMETRY_QUERY_EXPECTED_ROLES
            || grants != TELEMETRY_QUERY_EXPECTED_GRANTS
            || settings.trim() != "0"
        {
            return Ok(Some(format!(
                "the ClickHouse account {user} has other roles, grants or settings than \
                 0008_telemetry_query.sql gives it; an administrator must revoke them \
                 (docs/runbooks/migrations.md)"
            )));
        }
        Ok(None)
    }

    /// ingest's account ([`INGEST_ACCOUNT`], Laysi/trustiot-v2#642). When the run manages its
    /// password (the service is deployed): the ingest role must hold exactly what the DDL files
    /// gave it and be held by the account alone, and the account must hold that role alone, as
    /// its default role; then the run creates the account with that role and the password, or
    /// sets the password and lets it log in. Anything else revokes the account's login, if it
    /// exists, and stops the run for an administrator: the account writes and deletes every
    /// tenant's telemetry. Once an administrator has repaired it, the next run lets it log in
    /// again. When the run does not manage it (the service is off), an existing account cannot
    /// log in at all (`HOST NONE`), so the password a kept Secret still holds writes nothing.
    async fn ensure_ingest(
        &self,
        password: Option<&Password>,
        report: &mut Report,
    ) -> Result<(), Error> {
        let user = INGEST_ACCOUNT;
        let params = [("user", user)];
        let exists = self
            .query("look for the ingest account", USER_EXISTS, &params)
            .await?
            .trim()
            != "0";
        let Some(password) = password else {
            if exists && self.can_log_in(user).await? {
                self.query(&format!("revoke the login of {user}"), LOGIN_NONE, &params)
                    .await?;
                report.change(format!(
                    "revoked the login of ClickHouse account {user}: ingest is not deployed"
                ));
            }
            return Ok(());
        };
        if let Some(finding) = self.audit_ingest(exists).await? {
            if !exists {
                return Err(Error::ClickHouse(finding));
            }
            // Detection alone would leave the account able to act until an administrator reads
            // the log.
            let revoked = self
                .query(&format!("revoke the login of {user}"), LOGIN_NONE, &params)
                .await;
            return Err(Error::ClickHouse(match revoked {
                Ok(_) => format!(
                    "{finding}; the run revoked the login of {user} (HOST NONE), and the first \
                     run after the repair lets it log in again while ingest is deployed"
                ),
                Err(error) => {
                    format!("{finding}; revoking the login of {user} failed as well: {error}")
                }
            }));
        }
        let (hash, salt) = password
            .clickhouse_sha256()
            .map_err(|error| Error::Random(error.to_string()))?;
        let auth = [
            ("user", user),
            ("hash", hash.as_str()),
            ("salt", salt.as_str()),
        ];
        if !exists {
            self.query("create the ingest account", CREATE_INGEST, &auth)
                .await?;
            report.change(format!("created ClickHouse account {user}"));
            return Ok(());
        }
        if !self.can_log_in(user).await? {
            // Its login revoked by a run without the service, or after a finding: the Secret's
            // password first, so it never logs in again with an older one.
            self.query("set the ingest account's password", SET_PASSWORD, &auth)
                .await?;
            report.change(format!("set the password of ClickHouse account {user}"));
            self.query(&format!("let {user} log in"), LOGIN_ANY, &params)
                .await?;
            report.change(format!("let ClickHouse account {user} log in"));
            return Ok(());
        }
        match self.send(user, password.expose(), PROBE, &[], &[]).await {
            Ok(_) => Ok(()),
            Err(failure) if failure.is_authentication() => {
                self.query("set the ingest account's password", SET_PASSWORD, &auth)
                    .await?;
                report.change(format!("set the password of ClickHouse account {user}"));
                Ok(())
            }
            Err(failure) => Err(Error::ClickHouse(format!(
                "log in as {user} to check its password: {}",
                failure.text.trim()
            ))),
        }
    }

    /// What about the ingest role, its holders or, when it `exists`, ingest's account differs
    /// from what the DDL files and the run gave them, if anything.
    async fn audit_ingest(&self, exists: bool) -> Result<Option<String>, Error> {
        let user = INGEST_ACCOUNT;
        let role = [("role", INGEST_ROLE)];
        let role_exists = self
            .query("look for the ingest role", ROLE_EXISTS, &role)
            .await?;
        if role_exists.trim() == "0" {
            return Ok(Some(format!(
                "the ClickHouse role {INGEST_ROLE} does not exist after the DDL files; \
                 0004_access.sql creates it"
            )));
        }
        let grants = self
            .query("read the ingest role's grants", INGEST_ROLE_GRANTS, &role)
            .await?;
        let roles = self
            .query("read the roles of the ingest role", ROLE_ROLES, &role)
            .await?;
        let grants: Vec<&str> = grants.lines().filter(|line| !line.is_empty()).collect();
        if grants != INGEST_EXPECTED_GRANTS || roles.trim() != "0" {
            return Ok(Some(format!(
                "the role {INGEST_ROLE} has other grants or roles than the DDL files give it \
                 (SELECT, INSERT and ALTER DELETE on trustiot.telemetry; SELECT and INSERT on \
                 trustiot.stream_commits, trustiot.telemetry_1m and trustiot.telemetry_1h); an \
                 administrator must revoke or grant them again (docs/runbooks/migrations.md)"
            )));
        }
        let grantees = self
            .query(
                "read who holds the ingest role",
                INGEST_GRANTEES,
                &[("role", INGEST_ROLE), ("user", user)],
            )
            .await?;
        let grantees: Vec<&str> = grantees.lines().filter(|line| !line.is_empty()).collect();
        if !grantees.is_empty() {
            let shown = grantees
                .iter()
                .take(GRANTEES_SHOWN)
                .copied()
                .collect::<Vec<_>>()
                .join(", ");
            return Ok(Some(format!(
                "the role {INGEST_ROLE} is held by {} grantee(s) besides {user}: {shown}; every \
                 holder writes and deletes every tenant's telemetry, so an administrator must \
                 revoke it (docs/runbooks/migrations.md)",
                grantees.len()
            )));
        }
        if !exists {
            return Ok(None);
        }
        let params = [("user", user)];
        let roles = self
            .query("read the ingest account's roles", USER_ROLES, &params)
            .await?;
        let grants = self
            .query("read the ingest account's grants", USER_GRANTS, &params)
            .await?;
        let settings = self
            .query("read the ingest account's settings", USER_SETTINGS, &params)
            .await?;
        let roles: Vec<&str> = roles.lines().filter(|line| !line.is_empty()).collect();
        let expected = format!("{INGEST_ROLE}\t1\t0");
        if roles != [expected.as_str()] || grants.trim() != "0" || settings.trim() != "0" {
            return Ok(Some(format!(
                "the ClickHouse account {user} has other roles, grants or settings than \
                 {INGEST_ROLE} as its default role; an administrator must revoke them \
                 (docs/runbooks/migrations.md)"
            )));
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bound_backfill_policy_is_named_with_whom_it_names() {
        assert_eq!(
            bound_policies(
                "telemetry_1h_backfill\t1\t[]\ntelemetry_1m_backfill\t0\t['admin','reader']\n"
            ),
            "trustiot.telemetry_1h_backfill (all), \
             trustiot.telemetry_1m_backfill (['admin','reader'])"
        );
    }

    #[test]
    fn the_backfill_stays_a_day_inside_the_views_bound() {
        // From today - days + 3 to today: one day fewer than the views compare.
        assert_eq!(backfill_day_count(180), 178);
        assert_eq!(backfill_day_count(31), 29);
        assert_eq!(backfill_day_count(MAX_RAW_RETENTION_DAYS), 3648);
        // Capped as the views cap it, and never below zero.
        assert_eq!(backfill_day_count(u32::MAX), 3648);
        assert_eq!(backfill_day_count(2), 0);
        assert_eq!(backfill_day_count(0), 0);
        // The bound of the views that count holds, in both views and on both sides of each.
        let views = crate::clickhouse_files::FILES
            .iter()
            .find(|file| file.name == "0010_telemetry_rollups.sql")
            .expect("the file of the aggregates")
            .sql;
        let bound = format!(
            "toDate(now('UTC')) - least({{raw_retention_days:UInt32}}, {MAX_RAW_RETENTION_DAYS}) \
             + 2) AND {{last_day:Date}}"
        );
        assert_eq!(views.matches(&bound).count(), 4, "{bound}");
    }

    #[test]
    fn splits_like_the_ingest_tests() {
        let text = "-- comment; with a semicolon\nCREATE TABLE a\n(x UInt8);\n\nGRANT x TO y;\n";
        assert_eq!(
            split_statements(text).expect("valid"),
            ["CREATE TABLE a\n(x UInt8)", "GRANT x TO y"]
        );
        assert!(split_statements("SELECT 1; SELECT 2;").is_err());
        assert!(split_statements("SELECT 1").is_err());
        assert!(split_statements(";\n").is_err());
    }

    #[test]
    fn reads_only_the_ttl_it_manages() {
        let files = "MergeTree PARTITION BY toYYYYMM(received_at) ORDER BY (tenant_id, device_id, \
                     point, ts) TTL toDateTime(received_at) + toIntervalDay(400) SETTINGS \
                     fsync_after_insert = 1, index_granularity = 8192";
        assert_eq!(managed_ttl_days(files, "received_at"), Ok(400));
        // As ClickHouse 26.8 stores the run's query parameter.
        let run = "ReplacingMergeTree PARTITION BY toYYYYMM(committed_at) ORDER BY (gateway_id, \
                   stream_id, seq) TTL toDateTime(committed_at) + \
                   toIntervalDay(_CAST(180, 'UInt32')) SETTINGS fsync_after_insert = 1";
        assert_eq!(managed_ttl_days(run, "committed_at"), Ok(180));
        assert_eq!(
            managed_ttl_days(
                "MergeTree ORDER BY a TTL toDateTime(r) + toIntervalDay(31)",
                "r"
            ),
            Ok(31)
        );
        for other in [
            "MergeTree ORDER BY a SETTINGS index_granularity = 8192",
            // Another column: the gateway's timestamp must never drive retention.
            "MergeTree ORDER BY a TTL toDateTime(ts) + toIntervalDay(180) SETTINGS x = 1",
            "MergeTree ORDER BY a TTL toDateTime(received_at) + toIntervalMonth(6) SETTINGS x = 1",
            "MergeTree ORDER BY a TTL toDateTime(received_at) + toIntervalDay(180), \
             toDateTime(received_at) + toIntervalDay(30) TO VOLUME 'cold' SETTINGS x = 1",
            "MergeTree ORDER BY a TTL toDateTime(received_at) + toIntervalDay(-1) SETTINGS x = 1",
            "MergeTree ORDER BY a TTL toDateTime(received_at) + toIntervalDay() SETTINGS x = 1",
            "MergeTree ORDER BY a TTL toDateTime(received_at) + toIntervalDay(_CAST(180, \
             'UInt64')) SETTINGS x = 1",
            "MergeTree ORDER BY a TTL toDateTime(received_at) + toIntervalDay(99999999999) \
             SETTINGS x = 1",
        ] {
            assert!(
                managed_ttl_days(other, "received_at").is_err(),
                "accepted: {other}"
            );
        }
    }

    #[test]
    fn reads_only_the_aggregate_ttl_it_manages() {
        // As 0010_telemetry_rollups.sql creates it and as the run's MODIFY TTL stores it.
        let file = "AggregatingMergeTree PARTITION BY toYYYYMM(received_day) ORDER BY (tenant_id, \
                    device_id, point, bucket, gateway_id, stream_id, received_day) TTL \
                    (received_day + toIntervalMonth(13)) + toIntervalDay(1) SETTINGS \
                    fsync_after_insert = 1, index_granularity = 8192";
        assert_eq!(managed_ttl_months(file, "received_day"), Ok(13));
        let run = "AggregatingMergeTree ORDER BY a TTL (received_day + \
                   toIntervalMonth(_CAST(48, 'UInt32'))) + toIntervalDay(1) SETTINGS x = 1";
        assert_eq!(managed_ttl_months(run, "received_day"), Ok(48));
        for other in [
            "AggregatingMergeTree ORDER BY a SETTINGS index_granularity = 8192",
            // Another column: the bucket is the gateway's time and must never drive retention.
            "AggregatingMergeTree ORDER BY a TTL (bucket + toIntervalMonth(13)) + \
             toIntervalDay(1) SETTINGS x = 1",
            // Without the day, a cell would expire up to a day before its last reading is the
            // months old; in days, the unit the settings are not in.
            "AggregatingMergeTree ORDER BY a TTL received_day + toIntervalMonth(13) SETTINGS x = 1",
            "AggregatingMergeTree ORDER BY a TTL (received_day + toIntervalDay(400)) + \
             toIntervalDay(1) SETTINGS x = 1",
            "AggregatingMergeTree ORDER BY a TTL (received_day + toIntervalMonth(13)) + \
             toIntervalDay(2) SETTINGS x = 1",
            "AggregatingMergeTree ORDER BY a TTL (received_day + toIntervalMonth(13)) + \
             toIntervalDay(1), received_day + toIntervalMonth(1) TO VOLUME 'cold' SETTINGS x = 1",
            "AggregatingMergeTree ORDER BY a TTL (received_day + toIntervalMonth(-1)) + \
             toIntervalDay(1) SETTINGS x = 1",
            "AggregatingMergeTree ORDER BY a TTL (received_day + toIntervalMonth(_CAST(13, \
             'UInt64'))) + toIntervalDay(1) SETTINGS x = 1",
        ] {
            assert!(
                managed_ttl_months(other, "received_day").is_err(),
                "accepted: {other}"
            );
        }
        // The day parser does not take the month form, nor the other way round.
        assert!(managed_ttl_days(file, "received_day").is_err());
        assert!(
            managed_ttl_months(
                "MergeTree ORDER BY a TTL toDateTime(received_at) + toIntervalDay(180)",
                "received_at"
            )
            .is_err()
        );
    }

    /// Whether each table keeps its rows at least as long as the next, the months of the
    /// aggregates counted as their shortest span in days.
    fn ordered(state: [u32; 4]) -> bool {
        let [hour, minute, commits, telemetry] = state;
        hour >= minute
            && crate::retention::min_days_in_months(minute) >= commits
            && commits >= telemetry
    }

    #[test]
    fn retention_changes_keep_every_table_at_least_as_long_as_the_next() {
        let values = [
            [48, 13, 180, 180],
            [13, 13, 31, 31],
            [120, 120, 3650, 3650],
            [60, 24, 400, 400],
            [48, 14, 365, 365],
            [24, 13, 393, 393],
        ];
        for current in values {
            for targets in values {
                let mut state = current;
                for (index, from, to) in retention_steps(current, targets) {
                    assert_eq!(state[index], from, "{current:?} -> {targets:?}");
                    state[index] = to;
                    assert!(
                        ordered(state),
                        "{current:?} -> {targets:?} passes through {state:?}"
                    );
                }
                assert_eq!(state, targets, "{current:?} -> {targets:?}");
            }
        }
        // The order the run's report shows: longer from the longest-lived table down, shorter
        // from the shortest-lived one up; unchanged tables are left alone.
        assert_eq!(
            retention_steps([48, 13, 180, 180], [60, 24, 400, 400]),
            [(0, 48, 60), (1, 13, 24), (2, 180, 400), (3, 180, 400)]
        );
        assert_eq!(
            retention_steps([60, 24, 400, 400], [48, 13, 180, 180]),
            [(3, 400, 180), (2, 400, 180), (1, 24, 13), (0, 60, 48)]
        );
        assert_eq!(retention_steps([48, 13, 180, 180], [48, 13, 180, 180]), []);
    }

    #[test]
    fn every_product_file_splits() {
        for migration in crate::registry::PRODUCT.clickhouse {
            let statements = split_statements(migration.sql)
                .unwrap_or_else(|problem| panic!("{}: {problem}", migration.name));
            assert!(!statements.is_empty(), "{}", migration.name);
        }
    }
}
