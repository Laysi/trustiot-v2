//! ClickHouse itself keeps every tenant query inside the queryable period
//! (Laysi/trustiot-v2#143, acceptance 2; owner decision of 2026-10-26;
//! services/ingest/migrations/clickhouse/0011_queryable_period.sql).
//!
//! The service creates the tenant accounts as in production (raw readings 7 days, one-minute and
//! one-hour cells 3 months) and is then stopped: the test logs in as those accounts directly, with
//! neither the service nor the AI's SQL check in front, so what passes here is what the database
//! alone enforces. Readings of tenants A and B and of a tenant that exists only in ClickHouse are
//! written at several ages around the windows, each age with a marker of its own, so a reading or
//! cell outside the window, or of another tenant, shows as its marker in an answer or an error.
//! Then:
//! 1. the nine tenant row policies carry the window of their table;
//! 2. each view shows exactly the markers of its window, to A and to B;
//! 3. the routes to older rows (filters, aliases that shadow the filtered columns, joins,
//!    subqueries, set operations, window functions, table functions) answer without them or are
//!    refused for the reason recorded here, as A;
//! 4. settings in the query, the URL or a session, and other identities, cannot widen the window,
//!    and no expression of the caller is evaluated on a hidden row, as B;
//! 5. the window's start moves with the server's clock, query by query;
//! 6. windows changed by the service's account apply from the account's next query, wider and
//!    narrower, per table;
//! 7. an account without the windows, or with a broken one, reads nothing.
//!
//! Needs podman or docker; skipped without one unless `CI` is set. `--nocapture` prints every
//! route and its outcome for the test report.

mod support;

use std::time::{Duration, Instant};

use contract::{ServiceName, TenantRole};
use support::clickhouse::{Account, QueryError};
use support::{
    ClickHouseTarget, SERVICE_ACCOUNT, Service, Stack, random_password, wait_account_ready,
};
use telemetry_query::period::View;
use testkit::identity::TestIssuer;

/// Seconds in a day.
const DAY: i64 = 86_400;

/// A tenant that exists only in ClickHouse, as if it were another customer.
const VICTIM: &str = "0199a0b0-c0d0-7e0f-8a1b-2c3d4e5f6a7d";

/// The readings' ages, in seconds before they are written (negative: after), by name. Each age
/// gets three readings one millisecond apart, of every tenant.
const AGES: [(&str, i64); 8] = [
    ("FRESH", 3_600),
    ("D006", 6 * DAY),
    // An hour inside the raw window: the checks that rely on it run minutes after the insert.
    ("EDGEIN", 7 * DAY - 3_600),
    // Two minutes outside the raw window, and further out with every second.
    ("EDGEOUT", 7 * DAY + 120),
    ("D008", 8 * DAY),
    // Inside three calendar months (at least 89 days), outside one (at most 31 days).
    ("D040", 40 * DAY),
    // Outside three calendar months (at most 92 days).
    ("D100", 100 * DAY),
    ("FUTURE", -DAY),
];

/// The ages the raw view shows with the service's windows.
const RAW_VISIBLE: &[&str] = &["FRESH", "D006", "EDGEIN", "FUTURE"];
/// The ages both aggregate views show with the service's windows: a cell's `bucket` is the
/// reading time truncated, so the edge readings stay well inside three months.
const CELLS_VISIBLE: &[&str] = &[
    "FRESH", "D006", "EDGEIN", "EDGEOUT", "D008", "D040", "FUTURE",
];
/// Every age written but the future one is in the past; all of them, for the widest windows.
const ALL_AGES: &[&str] = &[
    "FRESH", "D006", "EDGEIN", "EDGEOUT", "D008", "D040", "D100", "FUTURE",
];
/// The ages the one-hour view shows with a window of one month.
const HOUR_ONE_MONTH: &[&str] = &["FRESH", "D006", "EDGEIN", "EDGEOUT", "D008", "FUTURE"];
/// No marker required.
const NOTHING: &[&str] = &[];

const RAW: &[View] = &[View::Raw];
const MINUTE: &[View] = &[View::Minute];
const HOUR: &[View] = &[View::Hour];

/// Readings at the server's clock minus `age` seconds, `step` milliseconds apart, with
/// `marker` as their text value. Written as the admin, as the ingest service writes them; the
/// materialized views of 0010_telemetry_rollups.sql fill both aggregate tables from them.
const INSERT_AGED: &str = "INSERT INTO trustiot.telemetry (tenant_id, gateway_id, stream_id,
  batch_seq, device_id, point, ts, received_at, value_type, value_string)
SELECT {tenant:UUID}, generateUUIDv4(), generateUUIDv4(), number, {device:UUID}, {point:String},
  now64(3, 'UTC') - toIntervalSecond({age:Int64}) + toIntervalMillisecond(number * {step:UInt64}),
  now64(3, 'UTC'), 'string', {marker:String}
FROM numbers({n:UInt64})";

/// Every `status` reading written, as the reference account counts them.
const ALL_STATUS_ROWS: &str =
    "SELECT count() FROM trustiot.telemetry WHERE point = 'status' FORMAT TSV";

/// The markers each view shows to the account. `.6` is `value_string` of a cell's first and last
/// reading (0010_telemetry_rollups.sql).
const RAW_MARKERS: &str = "SELECT DISTINCT value_string FROM trustiot.tenant_telemetry
WHERE point = 'status' ORDER BY 1 FORMAT TSV";
const MINUTE_MARKERS: &str = "SELECT DISTINCT arrayJoin([first.6, last.6])
FROM trustiot.tenant_telemetry_1m WHERE point = 'status' ORDER BY 1 FORMAT TSV";
const HOUR_MARKERS: &str = "SELECT DISTINCT arrayJoin([first.6, last.6])
FROM trustiot.tenant_telemetry_1h WHERE point = 'status' ORDER BY 1 FORMAT TSV";

/// The account's windows: raw days, one-minute months, one-hour months.
const WINDOWS: &str = "SELECT getSetting('SQL_queryable_raw_days'),
  getSetting('SQL_queryable_1m_months'), getSetting('SQL_queryable_1h_months')
FORMAT TSV";

/// The row policies of the tenant role and of the tenant views' definer, unescaped.
const TENANT_POLICIES: &str = "SELECT table, short_name, select_filter FROM system.row_policies
WHERE database = 'trustiot' AND short_name IN ('tenant_isolation', 'tenant_view_definer')
ORDER BY table, short_name FORMAT TSVRaw";

/// Window changes in the form the service's account writes them (src/accounts.rs,
/// `SET_WINDOWS`): a plan change.
const WIDEN: &str = "ALTER USER {user:Identifier} MODIFY SETTINGS
  SQL_queryable_raw_days = 365 CONST, SQL_queryable_1m_months = 12 CONST,
  SQL_queryable_1h_months = 12 CONST";
const NARROW_HOUR: &str = "ALTER USER {user:Identifier} MODIFY SETTINGS
  SQL_queryable_raw_days = 7 CONST, SQL_queryable_1m_months = 3 CONST,
  SQL_queryable_1h_months = 1 CONST";
const RESTORE: &str = "ALTER USER {user:Identifier} MODIFY SETTINGS
  SQL_queryable_raw_days = 7 CONST, SQL_queryable_1m_months = 3 CONST,
  SQL_queryable_1h_months = 3 CONST";

/// A tenant account as an administrator could leave it: the tenant role and the tenant, no
/// windows. `TENANT_SELF` becomes tenant A's ID (a `SETTINGS` value takes no query parameter).
const CREATE_WITHOUT_PERIOD: &str = "CREATE USER {user:Identifier}
IDENTIFIED WITH sha256_password BY {password:String} HOST ANY
DEFAULT ROLE trustiot_tenant_reader
SETTINGS PROFILE 'trustiot_tenant_reader', SQL_tenant_id = 'TENANT_SELF' CONST";
/// The same account with a raw window of `WINDOW_VALUE` and valid aggregate windows.
const BREAK_RAW_WINDOW: &str = "ALTER USER {user:Identifier}
SETTINGS PROFILE 'trustiot_tenant_reader', SQL_tenant_id = 'TENANT_SELF' CONST,
  SQL_queryable_raw_days = WINDOW_VALUE CONST, SQL_queryable_1m_months = 3 CONST,
  SQL_queryable_1h_months = 3 CONST";
const WITHOUT_PERIOD: &str = "trustiot_test_account_without_period";

/// Readings spread over the five minutes after the raw window's start, ten seconds apart.
const EDGE_READINGS: u64 = 30;
const EDGE_STEP_MS: u64 = 10_000;
const EDGE_COUNT: &str =
    "SELECT count() FROM trustiot.tenant_telemetry WHERE point = 'edge' FORMAT TSV";
/// The longest the window's start may take to pass the next edge reading, ten seconds away.
/// Generous: test hosts are often loaded (CLAUDE.md, "Test time limits").
const MOVE_LIMIT: Duration = Duration::from_secs(120);

/// What a route must do.
#[derive(Clone, Copy)]
enum Outcome {
    /// Answer, with at least the caller's markers of these ages.
    Answers(&'static [&'static str]),
    /// Be refused with this ClickHouse error.
    Refused(&'static str),
}

/// One query as a tenant account. In `sql`, `TENANT_SELF` becomes the caller's tenant ID,
/// `ACCOUNT_SELF` its account's name and `VICTIM_ID` [`VICTIM`].
struct Route {
    name: &'static str,
    sql: &'static str,
    /// The views it reads: the caller's markers they show may appear.
    views: &'static [View],
    /// Settings sent in the URL.
    url: &'static [(&'static str, &'static str)],
    outcome: Outcome,
}

const fn answers(
    name: &'static str,
    sql: &'static str,
    views: &'static [View],
    ages: &'static [&'static str],
) -> Route {
    Route {
        name,
        sql,
        views,
        url: &[],
        outcome: Outcome::Answers(ages),
    }
}

const fn refused(
    name: &'static str,
    sql: &'static str,
    views: &'static [View],
    code: &'static str,
) -> Route {
    Route {
        name,
        sql,
        views,
        url: &[],
        outcome: Outcome::Refused(code),
    }
}

impl Route {
    const fn with_url(mut self, url: &'static [(&'static str, &'static str)]) -> Self {
        self.url = url;
        self
    }
}

/// Routes to readings and cells outside the window, run as A. The outcomes are those of
/// ClickHouse 26.8 (the pinned image); a refusal must be for the recorded reason, so a typo
/// cannot pass for one.
const ROUTES: &[Route] = &[
    answers(
        "plain raw",
        "SELECT value_string FROM trustiot.tenant_telemetry",
        RAW,
        RAW_VISIBLE,
    ),
    answers(
        "raw older than the window",
        "SELECT value_string FROM trustiot.tenant_telemetry WHERE ts < now() - INTERVAL 7 DAY",
        RAW,
        NOTHING,
    ),
    answers(
        "raw since 1970",
        "SELECT value_string FROM trustiot.tenant_telemetry WHERE ts >= '1970-01-01 00:00:00'",
        RAW,
        RAW_VISIBLE,
    ),
    answers(
        "raw OR true",
        "SELECT value_string FROM trustiot.tenant_telemetry WHERE ts < now() - INTERVAL 7 DAY OR 1",
        RAW,
        RAW_VISIBLE,
    ),
    refused(
        "raw PREWHERE older",
        "SELECT value_string FROM trustiot.tenant_telemetry PREWHERE ts < now() - INTERVAL 7 DAY",
        RAW,
        "ILLEGAL_PREWHERE",
    ),
    answers(
        "raw FINAL older",
        "SELECT value_string FROM trustiot.tenant_telemetry FINAL WHERE ts < now() - INTERVAL 7 DAY",
        RAW,
        NOTHING,
    ),
    answers(
        "raw toDate",
        "SELECT value_string FROM trustiot.tenant_telemetry WHERE toDate(ts) < today() - 7",
        RAW,
        NOTHING,
    ),
    answers(
        "raw dateDiff",
        "SELECT value_string FROM trustiot.tenant_telemetry WHERE dateDiff('day', ts, now()) > 7",
        RAW,
        NOTHING,
    ),
    answers(
        "raw toUnixTimestamp",
        "SELECT value_string FROM trustiot.tenant_telemetry
         WHERE toUnixTimestamp(ts) < toUnixTimestamp(now()) - 7 * 86400",
        RAW,
        NOTHING,
    ),
    answers(
        "raw BETWEEN",
        "SELECT value_string FROM trustiot.tenant_telemetry
         WHERE ts BETWEEN now() - INTERVAL 200 DAY AND now() - INTERVAL 7 DAY",
        RAW,
        NOTHING,
    ),
    answers(
        "raw table alias",
        "SELECT t.value_string FROM trustiot.tenant_telemetry AS t
         WHERE t.ts < now() - INTERVAL 7 DAY",
        RAW,
        NOTHING,
    ),
    answers(
        "raw alias shadowing ts",
        "SELECT value_string, toDateTime64('2099-01-01 00:00:00', 3, 'UTC') AS ts
         FROM trustiot.tenant_telemetry",
        RAW,
        RAW_VISIBLE,
    ),
    answers(
        "raw alias shadowing tenant_id",
        "SELECT value_string, toUUID('TENANT_SELF') AS tenant_id FROM trustiot.tenant_telemetry",
        RAW,
        RAW_VISIBLE,
    ),
    answers(
        "raw WITH constant",
        "WITH toDateTime64('2000-01-01 00:00:00', 3, 'UTC') AS cutoff
         SELECT value_string FROM trustiot.tenant_telemetry WHERE ts > cutoff",
        RAW,
        RAW_VISIBLE,
    ),
    answers(
        "raw WITH shadowing ts",
        "WITH toDateTime64('2099-01-01 00:00:00', 3, 'UTC') AS ts
         SELECT value_string FROM trustiot.tenant_telemetry",
        RAW,
        RAW_VISIBLE,
    ),
    answers(
        "subquery",
        "SELECT value_string FROM (SELECT * FROM trustiot.tenant_telemetry)
         WHERE ts < now() - INTERVAL 7 DAY",
        RAW,
        NOTHING,
    ),
    answers(
        "nested subqueries",
        "SELECT v FROM (SELECT value_string AS v, ts FROM (SELECT * FROM trustiot.tenant_telemetry
         WHERE ts < now() - INTERVAL 7 DAY))",
        RAW,
        NOTHING,
    ),
    answers(
        "common table expression",
        "WITH old AS (SELECT value_string, ts FROM trustiot.tenant_telemetry
         WHERE ts < now() - INTERVAL 7 DAY) SELECT value_string FROM old",
        RAW,
        NOTHING,
    ),
    answers(
        "chained common table expressions",
        "WITH a AS (SELECT * FROM trustiot.tenant_telemetry),
         b AS (SELECT * FROM a WHERE ts < now() - INTERVAL 7 DAY) SELECT value_string FROM b",
        RAW,
        NOTHING,
    ),
    answers(
        "self JOIN",
        "SELECT b.value_string FROM trustiot.tenant_telemetry AS a
         JOIN trustiot.tenant_telemetry AS b ON a.device_id = b.device_id
         WHERE b.ts < now() - INTERVAL 7 DAY",
        RAW,
        NOTHING,
    ),
    answers(
        "CROSS JOIN",
        "SELECT b.value_string FROM trustiot.tenant_telemetry AS a
         CROSS JOIN trustiot.tenant_telemetry AS b",
        RAW,
        RAW_VISIBLE,
    ),
    answers(
        "LEFT JOIN ON true",
        "SELECT b.value_string FROM trustiot.tenant_telemetry AS a
         LEFT JOIN trustiot.tenant_telemetry AS b ON 1 = 1",
        RAW,
        RAW_VISIBLE,
    ),
    answers(
        "FULL JOIN",
        "SELECT a.value_string, b.value_string FROM trustiot.tenant_telemetry AS a
         FULL OUTER JOIN trustiot.tenant_telemetry AS b ON a.ts = b.ts",
        RAW,
        RAW_VISIBLE,
    ),
    answers(
        "ASOF JOIN",
        "SELECT b.value_string FROM trustiot.tenant_telemetry AS a
         ASOF JOIN trustiot.tenant_telemetry AS b ON a.device_id = b.device_id AND a.ts >= b.ts",
        RAW,
        RAW_VISIBLE,
    ),
    answers(
        "PASTE JOIN",
        "SELECT b.value_string FROM (SELECT value_string FROM trustiot.tenant_telemetry) AS a
         PASTE JOIN (SELECT value_string FROM trustiot.tenant_telemetry) AS b",
        RAW,
        RAW_VISIBLE,
    ),
    answers(
        "raw JOIN one-hour cells",
        "SELECT r.value_string, toString(h.last) FROM trustiot.tenant_telemetry AS r
         JOIN trustiot.tenant_telemetry_1h AS h ON r.device_id = h.device_id",
        &[View::Raw, View::Hour],
        RAW_VISIBLE,
    ),
    answers(
        "IN subquery",
        "SELECT value_string FROM trustiot.tenant_telemetry WHERE value_string IN
         (SELECT value_string FROM trustiot.tenant_telemetry WHERE ts < now() - INTERVAL 7 DAY)",
        RAW,
        NOTHING,
    ),
    answers(
        "IN tuple subquery",
        "SELECT value_string FROM trustiot.tenant_telemetry
         WHERE (device_id, ts) IN (SELECT device_id, ts FROM trustiot.tenant_telemetry)",
        RAW,
        RAW_VISIBLE,
    ),
    answers(
        "GLOBAL IN",
        "SELECT value_string FROM trustiot.tenant_telemetry
         WHERE value_string GLOBAL IN (SELECT value_string FROM trustiot.tenant_telemetry)",
        RAW,
        RAW_VISIBLE,
    ),
    answers(
        "scalar subquery",
        "SELECT (SELECT groupArray(value_string) FROM trustiot.tenant_telemetry)",
        RAW,
        RAW_VISIBLE,
    ),
    answers(
        "scalar subquery older",
        "SELECT (SELECT groupArray(value_string) FROM trustiot.tenant_telemetry
         WHERE ts < now() - INTERVAL 7 DAY)",
        RAW,
        NOTHING,
    ),
    answers(
        "arrayJoin of a scalar subquery",
        "SELECT arrayJoin((SELECT groupArray(value_string) FROM trustiot.tenant_telemetry))",
        RAW,
        RAW_VISIBLE,
    ),
    answers(
        "arrayJoin of tuples",
        "SELECT arrayJoin(groupArray((ts, value_string))) FROM trustiot.tenant_telemetry",
        RAW,
        RAW_VISIBLE,
    ),
    answers(
        "UNION ALL",
        "SELECT value_string FROM trustiot.tenant_telemetry WHERE ts < now() - INTERVAL 7 DAY
         UNION ALL SELECT value_string FROM trustiot.tenant_telemetry",
        RAW,
        RAW_VISIBLE,
    ),
    answers(
        "UNION DISTINCT",
        "SELECT value_string FROM trustiot.tenant_telemetry UNION DISTINCT
         SELECT value_string FROM trustiot.tenant_telemetry WHERE ts < now() - INTERVAL 7 DAY",
        RAW,
        RAW_VISIBLE,
    ),
    answers(
        "INTERSECT",
        "SELECT value_string FROM trustiot.tenant_telemetry
         INTERSECT SELECT value_string FROM trustiot.tenant_telemetry",
        RAW,
        RAW_VISIBLE,
    ),
    answers(
        "EXCEPT",
        "SELECT value_string FROM trustiot.tenant_telemetry EXCEPT SELECT 'x'",
        RAW,
        RAW_VISIBLE,
    ),
    answers(
        "window function over neighbours",
        "SELECT value_string, any(value_string) OVER
         (ORDER BY ts ROWS BETWEEN 100 PRECEDING AND 100 FOLLOWING)
         FROM trustiot.tenant_telemetry",
        RAW,
        RAW_VISIBLE,
    ),
    answers(
        "lagInFrame",
        "SELECT lagInFrame(value_string, 1) OVER (ORDER BY ts) FROM trustiot.tenant_telemetry",
        RAW,
        NOTHING,
    ),
    refused(
        "neighbor",
        "SELECT neighbor(value_string, -1) FROM trustiot.tenant_telemetry",
        RAW,
        "DEPRECATED_FUNCTION",
    ),
    answers(
        "WITH TOTALS",
        "SELECT value_string, count() FROM trustiot.tenant_telemetry
         GROUP BY value_string WITH TOTALS",
        RAW,
        RAW_VISIBLE,
    ),
    answers(
        "ROLLUP",
        "SELECT device_id, value_string, count() FROM trustiot.tenant_telemetry
         GROUP BY ROLLUP(device_id, value_string)",
        RAW,
        RAW_VISIBLE,
    ),
    answers(
        "LIMIT BY",
        "SELECT value_string FROM trustiot.tenant_telemetry ORDER BY ts LIMIT 1 BY value_string",
        RAW,
        RAW_VISIBLE,
    ),
    // The oldest reading the account can reach is the one just inside the window.
    answers(
        "argMin by ts",
        "SELECT min(ts), argMin(value_string, ts) FROM trustiot.tenant_telemetry",
        RAW,
        &["EDGEIN"],
    ),
    answers(
        "groupArray of everything",
        "SELECT groupArray(value_string) FROM trustiot.tenant_telemetry",
        RAW,
        RAW_VISIBLE,
    ),
    answers(
        "one-minute cells",
        "SELECT toString(last), toString(first) FROM trustiot.tenant_telemetry_1m",
        MINUTE,
        CELLS_VISIBLE,
    ),
    answers(
        "one-minute cells older than the window",
        "SELECT toString(last) FROM trustiot.tenant_telemetry_1m
         WHERE bucket < now() - INTERVAL 3 MONTH",
        MINUTE,
        NOTHING,
    ),
    answers(
        "one-minute tuple elements",
        "SELECT last.6, first.6 FROM trustiot.tenant_telemetry_1m",
        MINUTE,
        CELLS_VISIBLE,
    ),
    answers(
        "one-hour cells",
        "SELECT toString(last) FROM trustiot.tenant_telemetry_1h",
        HOUR,
        CELLS_VISIBLE,
    ),
    answers(
        "one-hour cells older than the window",
        "SELECT toString(last) FROM trustiot.tenant_telemetry_1h
         WHERE bucket < now() - INTERVAL 3 MONTH",
        HOUR,
        NOTHING,
    ),
    answers(
        "one-hour alias shadowing bucket",
        "SELECT toString(last), toDateTime('2099-01-01 00:00:00', 'UTC') AS bucket
         FROM trustiot.tenant_telemetry_1h",
        HOUR,
        CELLS_VISIBLE,
    ),
    answers(
        "one-minute JOIN one-hour cells",
        "SELECT toString(m.last) FROM trustiot.tenant_telemetry_1m AS m
         JOIN trustiot.tenant_telemetry_1h AS h ON m.device_id = h.device_id",
        &[View::Minute, View::Hour],
        CELLS_VISIBLE,
    ),
    answers(
        "UNION ALL of the three views",
        "SELECT value_string FROM trustiot.tenant_telemetry
         UNION ALL SELECT last.6 FROM trustiot.tenant_telemetry_1m
         UNION ALL SELECT last.6 FROM trustiot.tenant_telemetry_1h",
        &[View::Raw, View::Minute, View::Hour],
        CELLS_VISIBLE,
    ),
    answers(
        "view() table function",
        "SELECT * FROM view(SELECT value_string FROM trustiot.tenant_telemetry)",
        RAW,
        RAW_VISIBLE,
    ),
    answers(
        "merge() table function",
        "SELECT value_string FROM merge('trustiot', '^tenant_telemetry$')",
        RAW,
        RAW_VISIBLE,
    ),
    answers(
        "SAMPLE",
        "SELECT value_string FROM trustiot.tenant_telemetry SAMPLE 1",
        RAW,
        RAW_VISIBLE,
    ),
    refused(
        "raw base table",
        "SELECT value_string FROM trustiot.telemetry",
        RAW,
        "ACCESS_DENIED",
    ),
    refused(
        "one-minute base table",
        "SELECT tenant_id, bucket FROM trustiot.telemetry_1m",
        MINUTE,
        "ACCESS_DENIED",
    ),
    refused(
        "one-hour base table",
        "SELECT tenant_id, bucket FROM trustiot.telemetry_1h",
        HOUR,
        "ACCESS_DENIED",
    ),
];

/// Settings and other identities, run as B: each must be refused or keep the window.
const SETTINGS_ROUTES: &[Route] = &[
    refused(
        "SETTINGS a wider raw window",
        "SELECT value_string FROM trustiot.tenant_telemetry SETTINGS SQL_queryable_raw_days = 10000",
        RAW,
        "READONLY",
    ),
    refused(
        "SETTINGS readonly 0 and a wider window",
        "SELECT value_string FROM trustiot.tenant_telemetry
         SETTINGS readonly = 0, SQL_queryable_raw_days = 10000",
        RAW,
        "READONLY",
    ),
    // A SETTINGS clause in a subquery is ignored: the answer keeps the window.
    answers(
        "SETTINGS in a subquery",
        "SELECT value_string FROM (SELECT * FROM trustiot.tenant_telemetry
         SETTINGS SQL_queryable_raw_days = 10000)",
        RAW,
        RAW_VISIBLE,
    ),
    answers(
        "SETTINGS another tenant in a subquery",
        "SELECT value_string FROM (SELECT * FROM trustiot.tenant_telemetry
         SETTINGS SQL_tenant_id = 'VICTIM_ID')",
        RAW,
        RAW_VISIBLE,
    ),
    refused(
        "URL wider raw window",
        "SELECT value_string FROM trustiot.tenant_telemetry",
        RAW,
        "READONLY",
    )
    .with_url(&[("SQL_queryable_raw_days", "10000")]),
    refused(
        "URL wider one-hour window",
        "SELECT toString(last) FROM trustiot.tenant_telemetry_1h",
        HOUR,
        "READONLY",
    )
    .with_url(&[("SQL_queryable_1h_months", "1000")]),
    refused(
        "URL readonly 0 and a wider window",
        "SELECT value_string FROM trustiot.tenant_telemetry",
        RAW,
        "READONLY",
    )
    .with_url(&[("readonly", "0"), ("SQL_queryable_raw_days", "10000")]),
    refused(
        "SET a wider raw window",
        "SET SQL_queryable_raw_days = 10000",
        RAW,
        "READONLY",
    ),
    refused(
        "SETTINGS session_timezone",
        "SELECT value_string FROM trustiot.tenant_telemetry
         SETTINGS session_timezone = 'Pacific/Kiritimati'",
        RAW,
        "READONLY",
    ),
    refused(
        "URL session_timezone",
        "SELECT value_string FROM trustiot.tenant_telemetry",
        RAW,
        "READONLY",
    )
    .with_url(&[("session_timezone", "Pacific/Kiritimati")]),
    refused(
        "SETTINGS use_query_cache",
        "SELECT value_string FROM trustiot.tenant_telemetry SETTINGS use_query_cache = 1",
        RAW,
        "READONLY",
    ),
    refused(
        "ALTER USER CURRENT_USER",
        "ALTER USER CURRENT_USER SETTINGS SQL_queryable_raw_days = 10000",
        RAW,
        "ACCESS_DENIED",
    ),
    refused(
        "ALTER USER by its own name",
        "ALTER USER ACCOUNT_SELF MODIFY SETTINGS SQL_queryable_raw_days = 10000 CONST",
        RAW,
        "ACCESS_DENIED",
    ),
    refused(
        "CREATE SETTINGS PROFILE",
        "CREATE SETTINGS PROFILE wide SETTINGS SQL_queryable_raw_days = 10000 TO CURRENT_USER",
        RAW,
        "ACCESS_DENIED",
    ),
    refused(
        "ALTER SETTINGS PROFILE",
        "ALTER SETTINGS PROFILE trustiot_tenant_reader SETTINGS SQL_queryable_raw_days = 10000",
        RAW,
        "ACCESS_DENIED",
    ),
    refused(
        "ALTER ROW POLICY",
        "ALTER ROW POLICY tenant_isolation ON trustiot.tenant_telemetry USING 1",
        RAW,
        "ACCESS_DENIED",
    ),
    refused(
        "EXECUTE AS the views' definer",
        "EXECUTE AS trustiot_tenant_view_definer SELECT value_string FROM trustiot.telemetry",
        RAW,
        "ACCESS_DENIED",
    ),
    refused(
        "URL role of the ingest service",
        "SELECT tenant_id, bucket FROM trustiot.telemetry_1m",
        RAW,
        "SET_NON_GRANTED_ROLE",
    )
    .with_url(&[("role", "trustiot_ingest")]),
];

/// Expressions of the caller that would fail on a hidden row, run as B: none may be evaluated on
/// one (its error would carry the row's value).
const EVALUATION_ROUTES: &[Route] = &[
    answers(
        "throwIf on hidden raw rows in WHERE",
        "SELECT count() FROM trustiot.tenant_telemetry
         WHERE throwIf(ts < now64(3, 'UTC') - INTERVAL 7 DAY - INTERVAL 1 HOUR, 'hidden row')",
        RAW,
        NOTHING,
    ),
    refused(
        "throwIf on hidden raw rows in PREWHERE",
        "SELECT count() FROM trustiot.tenant_telemetry
         PREWHERE throwIf(ts < now64(3, 'UTC') - INTERVAL 7 DAY - INTERVAL 1 HOUR, 'hidden row')",
        RAW,
        "ILLEGAL_PREWHERE",
    ),
    answers(
        "throwIf on hidden raw rows in SELECT",
        "SELECT throwIf(ts < now64(3, 'UTC') - INTERVAL 7 DAY - INTERVAL 1 HOUR, 'hidden row')
         FROM trustiot.tenant_telemetry",
        RAW,
        NOTHING,
    ),
    answers(
        "throwIf on hidden one-minute cells",
        "SELECT count() FROM trustiot.tenant_telemetry_1m
         WHERE throwIf(bucket < now() - INTERVAL 3 MONTH - INTERVAL 1 DAY, 'hidden cell')",
        MINUTE,
        NOTHING,
    ),
    answers(
        "throwIf on hidden one-hour cells",
        "SELECT count() FROM trustiot.tenant_telemetry_1h
         WHERE throwIf(bucket < now() - INTERVAL 3 MONTH - INTERVAL 1 DAY, 'hidden cell')",
        HOUR,
        NOTHING,
    ),
    answers(
        "failing cast of hidden raw values",
        "SELECT count() FROM trustiot.tenant_telemetry
         WHERE ts < now() - INTERVAL 7 DAY AND toUInt8(value_string) = 0",
        RAW,
        NOTHING,
    ),
    // Fails on a visible cell's value: the error carries it, and only it.
    refused(
        "failing cast of one-hour values",
        "SELECT count() FROM trustiot.tenant_telemetry_1h WHERE toUInt8(last.6) = 0",
        HOUR,
        "CANNOT_PARSE_TEXT",
    ),
];

/// A tenant, the letter in its markers, and its ID.
struct Tenant {
    letter: char,
    id: String,
}

/// The marker of `letter`'s readings of age `age`.
fn marker(letter: char, age: &str) -> String {
    format!("MK_{letter}_{age}_")
}

/// The ages `view` shows with the service's windows.
fn visible(view: View) -> &'static [&'static str] {
    match view {
        View::Raw => RAW_VISIBLE,
        View::Minute | View::Hour => CELLS_VISIBLE,
    }
}

/// The markers that must not reach the caller `me` through `views`: every marker of the other
/// tenants, and the caller's own of the ages none of the views shows.
fn forbidden(me: char, views: &[View]) -> Vec<String> {
    let mut markers = Vec::new();
    for (age, _) in AGES {
        for letter in ['A', 'B', 'V'] {
            let shown = letter == me && views.iter().any(|view| visible(*view).contains(&age));
            if !shown {
                markers.push(marker(letter, age));
            }
        }
    }
    markers
}

/// The answer of a markers query that shows `ages` of `letter`: sorted, one per line.
fn markers_answer(letter: char, ages: &[&str]) -> String {
    let mut markers: Vec<String> = ages.iter().map(|age| marker(letter, age)).collect();
    markers.sort();
    markers.iter().map(|marker| format!("{marker}\n")).collect()
}

/// Checks `answer` of the query `name` against `outcome` and the markers `me` may see through
/// `views`; describes the outcome.
fn check(
    name: &str,
    views: &[View],
    outcome: Outcome,
    me: char,
    answer: &Result<String, QueryError>,
) -> String {
    let text = match answer {
        Ok(body) => body,
        Err(error) => &error.body,
    };
    let leaked: Vec<String> = forbidden(me, views)
        .into_iter()
        .filter(|marker| text.contains(marker.as_str()))
        .collect();
    assert!(
        leaked.is_empty(),
        "{name}: readings outside the period or of another tenant reached the caller: \
         {leaked:?}\n{text}"
    );
    match (outcome, answer) {
        (Outcome::Answers(ages), Ok(body)) => {
            for age in ages {
                let wanted = marker(me, age);
                assert!(
                    body.contains(&wanted),
                    "{name}: {wanted} missing from the answer:\n{body}"
                );
            }
            format!("answered, {} lines", body.lines().count())
        }
        (Outcome::Refused(code), Err(error)) => {
            assert_eq!(
                error.code(),
                code,
                "{name}: refused for another reason: {}",
                error.body
            );
            format!("refused: {code}")
        }
        (Outcome::Answers(_), Err(error)) => {
            panic!("{name}: refused instead of answering: {}", error.body)
        }
        (Outcome::Refused(code), Ok(body)) => {
            panic!("{name}: answered instead of being refused with {code}:\n{body}")
        }
    }
}

/// Runs `routes` as `account` of `me`.
#[allow(
    clippy::print_stderr,
    reason = "prints each route's outcome for the test report"
)]
async fn run_routes(stack: &Stack, me: &Tenant, account: &Account, routes: &[Route]) {
    for route in routes {
        let sql = route
            .sql
            .replace("TENANT_SELF", &me.id)
            .replace("ACCOUNT_SELF", &account.user)
            .replace("VICTIM_ID", VICTIM);
        let answer = stack.clickhouse.query_with(account, &sql, route.url).await;
        let shown = check(route.name, route.views, route.outcome, me.letter, &answer);
        eprintln!("period {:>42}: {shown}", route.name);
    }
}

/// Runs `sql` as `account`, which must answer.
async fn ok(stack: &Stack, account: &Account, sql: &str) -> String {
    stack
        .clickhouse
        .query(account, sql)
        .await
        .unwrap_or_else(|error| panic!("as {}: {sql}\n{}", account.user, error.body))
}

/// Checks the markers each view shows to `account` of `letter`.
async fn assert_views(
    stack: &Stack,
    account: &Account,
    letter: char,
    shown: [&[&str]; 3],
    when: &str,
) {
    for (query, ages, view) in [
        (RAW_MARKERS, shown[0], View::Raw),
        (MINUTE_MARKERS, shown[1], View::Minute),
        (HOUR_MARKERS, shown[2], View::Hour),
    ] {
        assert_eq!(
            ok(stack, account, query).await,
            markers_answer(letter, ages),
            "{}, {when}: the markers {} shows",
            account.user,
            view.table()
        );
    }
}

/// Readings to write with [`insert`].
struct Aged<'a> {
    tenant: &'a str,
    device: &'a str,
    point: &'a str,
    age_s: i64,
    step_ms: u64,
    count: u64,
    marker: &'a str,
}

async fn insert(stack: &Stack, aged: &Aged<'_>) {
    let (age, step, count) = (
        aged.age_s.to_string(),
        aged.step_ms.to_string(),
        aged.count.to_string(),
    );
    stack
        .clickhouse
        .query_with(
            &stack.clickhouse.admin,
            INSERT_AGED,
            &[
                ("param_tenant", aged.tenant),
                ("param_device", aged.device),
                ("param_point", aged.point),
                ("param_age", &age),
                ("param_step", &step),
                ("param_n", &count),
                ("param_marker", aged.marker),
            ],
        )
        .await
        .unwrap_or_else(|error| panic!("insert readings: {}", error.body));
}

/// Runs `sql` as the admin with `params`, which must answer.
async fn admin_with(stack: &Stack, sql: &str, params: &[(&str, &str)]) -> String {
    stack
        .clickhouse
        .query_with(&stack.clickhouse.admin, sql, params)
        .await
        .unwrap_or_else(|error| panic!("{sql}: {}", error.body))
}

/// The edge readings `account` sees.
async fn edge_count(stack: &Stack, account: &Account) -> u64 {
    ok(stack, account, EDGE_COUNT)
        .await
        .trim()
        .parse()
        .expect("a count")
}

/// 1. The nine tenant policies carry the window of their table.
async fn policies_carry_the_window(stack: &Stack) {
    let rows = stack.clickhouse.admin_ok(TENANT_POLICIES).await;
    let mut seen = Vec::new();
    for row in rows.lines() {
        let mut fields = row.splitn(3, '\t');
        let (Some(table), Some(name), Some(filter)) = (fields.next(), fields.next(), fields.next())
        else {
            panic!("a row policy: {row:?}");
        };
        let setting = if table.ends_with("_1m") {
            View::Minute.setting()
        } else if table.ends_with("_1h") {
            View::Hour.setting()
        } else {
            View::Raw.setting()
        };
        assert!(
            filter.contains("getSetting('SQL_tenant_id')") && filter.contains(setting),
            "{name} ON {table}: the tenant and {setting} in its filter: {filter}"
        );
        seen.push(format!("{table} {name}"));
    }
    assert_eq!(
        seen,
        [
            "telemetry tenant_isolation",
            "telemetry tenant_view_definer",
            "telemetry_1h tenant_isolation",
            "telemetry_1h tenant_view_definer",
            "telemetry_1m tenant_isolation",
            "telemetry_1m tenant_view_definer",
            "tenant_telemetry tenant_isolation",
            "tenant_telemetry_1h tenant_isolation",
            "tenant_telemetry_1m tenant_isolation",
        ],
        "the tenant policies"
    );
}

#[allow(
    clippy::print_stderr,
    reason = "prints the sessions, the moving start and the plan changes for the test report"
)]
#[tokio::test(flavor = "multi_thread")]
async fn the_database_keeps_tenant_queries_inside_the_queryable_period() {
    let Some(stack) = Stack::start().await else {
        return;
    };
    let issuer = TestIssuer::new();
    let service = Service::start(
        &stack.product,
        &ClickHouseTarget {
            url: &stack.clickhouse.url,
            password: &stack.service_password,
        },
        &issuer.jwks(),
    )
    .await;
    let a = stack.create_tenant(&issuer, "Plant A").await;
    let b = stack.create_tenant(&issuer, "Plant B").await;
    for tenant in [a, b] {
        let token = issuer.tenant_token(ServiceName::TelemetryQuery, tenant, TenantRole::Viewer);
        wait_account_ready(&service, &token, tenant).await;
    }
    // From here on, the database alone.
    drop(service);
    let (account_a, account_b) = (stack.tenant_account(a), stack.tenant_account(b));
    for account in [&account_a, &account_b] {
        assert_eq!(
            ok(&stack, account, WINDOWS).await,
            "7\t3\t3\n",
            "{}: the windows the service gives every account",
            account.user
        );
    }
    let tenant_a = Tenant {
        letter: 'A',
        id: a.to_string(),
    };
    let tenant_b = Tenant {
        letter: 'B',
        id: b.to_string(),
    };
    let victim = Tenant {
        letter: 'V',
        id: VICTIM.to_owned(),
    };

    // 1. The policies.
    policies_carry_the_window(&stack).await;

    // 2. Readings of every age for all three tenants, and what each view shows.
    let device = uuid::Uuid::new_v4().to_string();
    for tenant in [&tenant_a, &tenant_b, &victim] {
        for (age, seconds) in AGES {
            insert(
                &stack,
                &Aged {
                    tenant: &tenant.id,
                    device: &device,
                    point: "status",
                    age_s: seconds,
                    step_ms: 1,
                    count: 3,
                    marker: &marker(tenant.letter, age),
                },
            )
            .await;
        }
    }
    assert_eq!(
        stack.reference(ALL_STATUS_ROWS, &[]).await,
        format!("{}\n", 3 * 3 * AGES.len()),
        "every reading was written"
    );
    for (account, letter) in [(&account_a, 'A'), (&account_b, 'B')] {
        assert_views(
            &stack,
            account,
            letter,
            [RAW_VISIBLE, CELLS_VISIBLE, CELLS_VISIBLE],
            "with the service's windows",
        )
        .await;
    }

    // 3. The routes, as A.
    run_routes(&stack, &tenant_a, &account_a, ROUTES).await;

    // 4. Settings, identities and evaluation order, as B; then a session.
    run_routes(&stack, &tenant_b, &account_b, SETTINGS_ROUTES).await;
    assert_eq!(
        ok(
            &stack,
            &account_b,
            "SELECT * FROM (SELECT getSetting('SQL_queryable_raw_days')
             SETTINGS SQL_queryable_raw_days = 10000) FORMAT TSV"
        )
        .await,
        "7\n",
        "a SETTINGS clause in a subquery leaves the window as it is"
    );
    run_routes(&stack, &tenant_b, &account_b, EVALUATION_ROUTES).await;
    let session = uuid::Uuid::new_v4().simple().to_string();
    let in_session = [("session_id", session.as_str())];
    let set = stack
        .clickhouse
        .query_with(
            &account_b,
            "SET SQL_queryable_raw_days = 10000",
            &in_session,
        )
        .await;
    let shown = check("session: SET", RAW, Outcome::Refused("READONLY"), 'B', &set);
    eprintln!("period {:>42}: {shown}", "session: SET a wider window");
    let after = stack
        .clickhouse
        .query_with(
            &account_b,
            "SELECT value_string FROM trustiot.tenant_telemetry",
            &in_session,
        )
        .await;
    let shown = check(
        "session: SELECT after SET",
        RAW,
        Outcome::Answers(RAW_VISIBLE),
        'B',
        &after,
    );
    eprintln!("period {:>42}: {shown}", "session: SELECT after SET");
    stack
        .clickhouse
        .query_with(&account_b, "SET ROLE NONE", &in_session)
        .await
        .unwrap_or_else(|error| panic!("SET ROLE NONE in a session: {}", error.body));
    let after = stack
        .clickhouse
        .query_with(
            &account_b,
            "SELECT value_string FROM trustiot.tenant_telemetry",
            &in_session,
        )
        .await;
    let shown = check(
        "session: SELECT after SET ROLE NONE",
        RAW,
        Outcome::Refused("ACCESS_DENIED"),
        'B',
        &after,
    );
    eprintln!(
        "period {:>42}: {shown}",
        "session: SELECT after SET ROLE NONE"
    );

    // 5. The start of the raw window moves with the server's clock: readings ten seconds apart
    //    from just after it leave one by one, with nothing but time passing.
    insert(
        &stack,
        &Aged {
            tenant: &tenant_b.id,
            device: &device,
            point: "edge",
            age_s: 7 * DAY - 10,
            step_ms: EDGE_STEP_MS,
            count: EDGE_READINGS,
            marker: "MK_B_MOVING_",
        },
    )
    .await;
    let first = edge_count(&stack, &account_b).await;
    assert!(
        first > 0,
        "none of the edge readings inside the window, five minutes long, at the first count"
    );
    let deadline = Instant::now() + MOVE_LIMIT;
    let later = loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let now = edge_count(&stack, &account_b).await;
        if now < first {
            break now;
        }
        assert!(
            Instant::now() < deadline,
            "the raw window's start passing the next edge reading: still {now} of {first} after \
             {MOVE_LIMIT:?}"
        );
    };
    eprintln!("period: edge readings inside the raw window, later query: {first} -> {later}");

    // 6. Plan changes by the service's account apply from A's next query, per table.
    let service_account = Account {
        user: SERVICE_ACCOUNT.to_owned(),
        password: stack.service_password.clone(),
    };
    let user_a = account_a.user.clone();
    for (statement, windows, shown, when) in [
        (
            WIDEN,
            "365\t12\t12\n",
            [ALL_AGES, ALL_AGES, ALL_AGES],
            "after widening every window",
        ),
        (
            NARROW_HOUR,
            "7\t3\t1\n",
            [RAW_VISIBLE, CELLS_VISIBLE, HOUR_ONE_MONTH],
            "after narrowing the one-hour window to a month",
        ),
        (
            RESTORE,
            "7\t3\t3\n",
            [RAW_VISIBLE, CELLS_VISIBLE, CELLS_VISIBLE],
            "after restoring the service's windows",
        ),
    ] {
        stack
            .clickhouse
            .query_with(&service_account, statement, &[("param_user", &user_a)])
            .await
            .unwrap_or_else(|error| panic!("the service's account: {statement}\n{}", error.body));
        assert_eq!(ok(&stack, &account_a, WINDOWS).await, windows, "{when}");
        assert_views(&stack, &account_a, 'A', shown, when).await;
        eprintln!("period: plan change {when}: windows {}", windows.trim_end());
    }

    // 7. An account without the windows, or with a broken raw window, reads nothing.
    let without = Account {
        user: WITHOUT_PERIOD.to_owned(),
        password: random_password(),
    };
    admin_with(
        &stack,
        &CREATE_WITHOUT_PERIOD.replace("TENANT_SELF", &tenant_a.id),
        &[
            ("param_user", WITHOUT_PERIOD),
            ("param_password", &without.password),
        ],
    )
    .await;
    for (view, sql) in [
        (View::Raw, RAW_MARKERS),
        (View::Minute, MINUTE_MARKERS),
        (View::Hour, HOUR_MARKERS),
    ] {
        let answer = stack.clickhouse.query(&without, sql).await;
        let name = format!("no windows, {}", view.table());
        let shown = check(
            &name,
            &[view],
            Outcome::Refused("UNKNOWN_SETTING"),
            'A',
            &answer,
        );
        eprintln!("period {name:>42}: {shown}");
    }
    for (value, code) in [
        ("-1", "CANNOT_CONVERT_TYPE"),
        ("70000", "CANNOT_CONVERT_TYPE"),
        ("'abc'", "CANNOT_PARSE_TEXT"),
    ] {
        admin_with(
            &stack,
            &BREAK_RAW_WINDOW
                .replace("TENANT_SELF", &tenant_a.id)
                .replace("WINDOW_VALUE", value),
            &[("param_user", WITHOUT_PERIOD)],
        )
        .await;
        let answer = stack.clickhouse.query(&without, RAW_MARKERS).await;
        let name = format!("raw window {value}");
        let shown = check(&name, RAW, Outcome::Refused(code), 'A', &answer);
        eprintln!("period {name:>42}: {shown}");
    }
}
