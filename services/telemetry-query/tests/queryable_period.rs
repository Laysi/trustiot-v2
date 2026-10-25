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
use support::period::{
    AGES, Aged, CELLS_VISIBLE, DAY, EVALUATION_ROUTES, ORACLE_ROUTES, Outcome, RAW, RAW_VISIBLE,
    ROUTES, Route, SETTINGS_ROUTES, Tenant, VICTIM, forbidden, insert, marker,
};
use support::{
    ClickHouseTarget, SERVICE_ACCOUNT, Service, Stack, random_password, wait_account_ready,
};
use telemetry_query::period::View;
use testkit::identity::TestIssuer;

/// Every age written but the future one is in the past; all of them, for the widest windows.
const ALL_AGES: &[&str] = &[
    "FRESH", "D006", "EDGEIN", "EDGEOUT", "D008", "D040", "D100", "FUTURE",
];
/// The ages the one-hour view shows with a window of one month.
const HOUR_ONE_MONTH: &[&str] = &["FRESH", "D006", "EDGEIN", "EDGEOUT", "D008", "FUTURE"];

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
    run_routes(&stack, &tenant_b, &account_b, ORACLE_ROUTES).await;
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
