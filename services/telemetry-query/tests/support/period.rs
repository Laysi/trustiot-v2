//! The queryable period's corpus (Laysi/trustiot-v2#143, acceptance 2): readings of three
//! tenants at ages around the windows, each age with a marker of its own, and the routes to older
//! rows. `tests/queryable_period.rs` runs the routes as the tenant accounts directly, and
//! `tests/sql_api.rs` through the SQL endpoint.

use telemetry_query::period::View;

use super::Stack;

/// Seconds in a day.
pub const DAY: i64 = 86_400;

/// A tenant that exists only in ClickHouse, as if it were another customer.
pub const VICTIM: &str = "0199a0b0-c0d0-7e0f-8a1b-2c3d4e5f6a7d";

/// The readings' ages, in seconds before they are written (negative: after), by name. Each age
/// gets three readings one millisecond apart, of every tenant.
pub const AGES: [(&str, i64); 8] = [
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
pub const RAW_VISIBLE: &[&str] = &["FRESH", "D006", "EDGEIN", "FUTURE"];
/// The ages both aggregate views show with the service's windows: a cell's `bucket` is the
/// reading time truncated, so the edge readings stay well inside three months.
pub const CELLS_VISIBLE: &[&str] = &[
    "FRESH", "D006", "EDGEIN", "EDGEOUT", "D008", "D040", "FUTURE",
];
/// No marker required.
pub const NOTHING: &[&str] = &[];

pub const RAW: &[View] = &[View::Raw];
pub const MINUTE: &[View] = &[View::Minute];
pub const HOUR: &[View] = &[View::Hour];

/// Readings at the server's clock minus `age` seconds, `step` milliseconds apart, with
/// `marker` as their text value. Written as the admin, as the ingest service writes them; the
/// materialized views of 0010_telemetry_rollups.sql fill both aggregate tables from them.
pub const INSERT_AGED: &str = "INSERT INTO trustiot.telemetry (tenant_id, gateway_id, stream_id,
  batch_seq, device_id, point, ts, received_at, value_type, value_string)
SELECT {tenant:UUID}, generateUUIDv4(), generateUUIDv4(), number, {device:UUID}, {point:String},
  now64(3, 'UTC') - toIntervalSecond({age:Int64}) + toIntervalMillisecond(number * {step:UInt64}),
  now64(3, 'UTC'), 'string', {marker:String}
FROM numbers({n:UInt64})";

/// What a route must do.
#[derive(Clone, Copy)]
pub enum Outcome {
    /// Answer, with at least the caller's markers of these ages.
    Answers(&'static [&'static str]),
    /// Be refused with this ClickHouse error.
    Refused(&'static str),
}

/// One query as a tenant account. In `sql`, `TENANT_SELF` becomes the caller's tenant ID,
/// `ACCOUNT_SELF` its account's name and `VICTIM_ID` [`VICTIM`].
pub struct Route {
    pub name: &'static str,
    pub sql: &'static str,
    /// The views it reads: the caller's markers they show may appear.
    pub views: &'static [View],
    /// Settings sent in the URL.
    pub url: &'static [(&'static str, &'static str)],
    pub outcome: Outcome,
}

pub const fn answers(
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

pub const fn refused(
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
    pub const fn with_url(mut self, url: &'static [(&'static str, &'static str)]) -> Self {
        self.url = url;
        self
    }
}

/// Routes to readings and cells outside the window, run as A. The outcomes are those of
/// ClickHouse 26.8 (the pinned image); a refusal must be for the recorded reason, so a typo
/// cannot pass for one.
pub const ROUTES: &[Route] = &[
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
pub const SETTINGS_ROUTES: &[Route] = &[
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
pub const EVALUATION_ROUTES: &[Route] = &[
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
pub struct Tenant {
    pub letter: char,
    pub id: String,
}

/// The marker of `letter`'s readings of age `age`.
pub fn marker(letter: char, age: &str) -> String {
    format!("MK_{letter}_{age}_")
}

/// The ages `view` shows with the service's windows.
pub fn visible(view: View) -> &'static [&'static str] {
    match view {
        View::Raw => RAW_VISIBLE,
        View::Minute | View::Hour => CELLS_VISIBLE,
    }
}

/// The markers that must not reach the caller `me` through `views`: every marker of the other
/// tenants, and the caller's own of the ages none of the views shows.
pub fn forbidden(me: char, views: &[View]) -> Vec<String> {
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
/// Readings to write with [`insert`].
pub struct Aged<'a> {
    pub tenant: &'a str,
    pub device: &'a str,
    pub point: &'a str,
    pub age_s: i64,
    pub step_ms: u64,
    pub count: u64,
    pub marker: &'a str,
}

pub async fn insert(stack: &Stack, aged: &Aged<'_>) {
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
