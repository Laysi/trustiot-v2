//! The internal SQL endpoint against the real servers (Laysi/trustiot-v2#143): the service runs
//! as a real process on the product's Postgres schema and a throwaway ClickHouse with the
//! product's ClickHouse schema; tenants are created by the identity service's own code. The
//! calls carry tokens as the assistant's token exchange makes them (`act`).
//!
//! 1. A model's queries (`tests/data/model-queries.sql`) answer, with the checked SQL, the
//!    columns and their types, and where each view's period starts; every tenant role may call,
//!    only an AI service in a tenant may.
//! 2. Every refusal class answers its fixed details and nothing else: no SQL, no ClickHouse text
//!    (acceptance 3).
//! 3. The 2026-10-09 PoC's attack types (`support::poc`) are refused or answer without another
//!    tenant's readings (acceptance 1).
//! 4. The queryable period's routes (`support::period`) are refused or answer without readings
//!    outside the period or of another tenant (acceptance 2).
//! 5. The same SQL as two tenants answers each its own readings (acceptance 5).
//! 6. Statements other than a `SELECT` never reach ClickHouse: `system.query_log` holds none of
//!    them and only `SELECT`s of the tenant accounts (acceptance 4).
//! 7. ClickHouse parses each printed query as it parses the query as written (`EXPLAIN AST`),
//!    groups the operators of a printed query as sqlparser did where the two rank them
//!    differently, and every allowed function is ClickHouse's, with its case rule.
//!
//! Needs podman or docker; skipped without one unless `CI` is set. One test: the binary shares
//! one product database. `--nocapture` prints every outcome for the test report.

mod support;

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use contract::api::telemetry_query::{MAX_RESULT_BYTES, MAX_SQL_BYTES, RunSql};
use contract::client::Endpoint as _;
use contract::identity::Actor;
use contract::{ServiceName, TenantGrant, TenantId, TenantRole};
use serde_json::{Value, json};
use support::clickhouse::Account;
use support::period::{
    self, AGES, Aged, EVALUATION_ROUTES, Outcome, ROUTES, Route, SETTINGS_ROUTES, Tenant,
};
use support::poc::{A_MARKER, ATTACKS, B_MARKER, HEAVY, SYSTEM_TABLES, VICTIM, VICTIM_MARKER};
use support::{ClickHouseTarget, Readings, Service, Stack, wait_account_ready};
use telemetry_query::accounts::EXPECTED_WINDOWS;
use telemetry_query::period::View;
use telemetry_query::sql::functions::{COMBINATORS, FUNCTIONS, Kind};
use telemetry_query::sql::{CheckError, check};
use testkit::identity::TestIssuer;

/// Hourly temperature readings of tenant C, the model's data.
const TEMPERATURES: u64 = 48;

/// What every statement of the "never sent" list carries, so `system.query_log` can tell.
const PROBE: &str = "t143probe";

/// Statements that are not one `SELECT`. Each must be refused before anything reaches
/// ClickHouse.
const NOT_SENT: &[&str] = &[
    "SET ROLE NONE /* t143probe */",
    "SET DEFAULT ROLE NONE TO trustiot_tenant_reader /* t143probe */",
    "SET max_threads = 1 /* t143probe */",
    "SET SQL_queryable_raw_days = 10000 /* t143probe */",
    "INSERT INTO trustiot.telemetry SELECT * FROM trustiot.telemetry /* t143probe */",
    "ALTER TABLE trustiot.telemetry DELETE WHERE 1 /* t143probe */",
    "ALTER USER CURRENT_USER SETTINGS SQL_queryable_raw_days = 10000 /* t143probe */",
    "GRANT SELECT ON trustiot.telemetry TO trustiot_tenant_reader /* t143probe */",
    "DROP ROW POLICY tenant_isolation ON trustiot.tenant_telemetry /* t143probe */",
    "SYSTEM RELOAD CONFIG /* t143probe */",
    "KILL QUERY WHERE 1 /* t143probe */",
    "EXPLAIN SELECT 1 /* t143probe */",
    "SELECT 1 FROM tenant_telemetry; SET DEFAULT ROLE NONE TO x /* t143probe */",
    "SELECT 'x' FROM tenant_telemetry; SELECT 't143probe'",
    "SELECT point FROM tenant_telemetry SETTINGS readonly = 0 /* t143probe */",
];

/// Queries written as the check prints them, but for spacing, case, comments and parentheses:
/// ClickHouse must parse the printed text into the same tree (`EXPLAIN AST`).
const AS_WRITTEN: &[&str] = &[
    "select point, count() as n from trustiot.tenant_telemetry where value_float > 1 and not value_bool or point = 'a' group by point order by n desc limit 5",
    "SELECT - -1, - - - value_int, 1 - -1, - value_float FROM trustiot.tenant_telemetry",
    "SELECT value_float * 2 + 1, (value_float + 1) * 2, value_int % 3, value_int / 2 - 1 FROM trustiot.tenant_telemetry",
    "SELECT point FROM trustiot.tenant_telemetry WHERE value_float BETWEEN 1 AND 2 AND point LIKE 'a%' OR point NOT ILIKE '%b'",
    "SELECT point FROM trustiot.tenant_telemetry WHERE value_int IN (-1, 2, 3) AND (point, value_int) NOT IN (('a', 1))",
    "SELECT concat(point, '-') || 'x', 'it''s' FROM trustiot.tenant_telemetry",
    "SELECT arrayMap(x -> x * 2, [1, 2, 3]), arraySort((x, y) -> y, [1, 2], [2, 1])[1] FROM trustiot.tenant_telemetry_1m",
    "SELECT CASE WHEN value_float > 1 THEN 'high' WHEN value_float < 0 THEN 'low' ELSE 'mid' END FROM trustiot.tenant_telemetry",
    "SELECT toStartOfInterval(ts, INTERVAL 15 MINUTE, 'UTC'), now() - INTERVAL 1 DAY FROM trustiot.tenant_telemetry",
    "SELECT last.value_float, first.ts, tupleElement(last, 'value_int') FROM trustiot.tenant_telemetry_1h",
    "SELECT quantile(0.9)(value_float), quantiles(0.5, 0.9)(value_float), sumIf(value_float, value_float > 0) FROM trustiot.tenant_telemetry",
    "SELECT point, sum(readings) OVER (PARTITION BY point ORDER BY bucket ROWS BETWEEN 2 PRECEDING AND CURRENT ROW) FROM trustiot.tenant_telemetry_1m",
    "WITH d AS (SELECT toDate(bucket, 'UTC') AS day, sum(readings) AS n FROM trustiot.tenant_telemetry_1h GROUP BY day) SELECT day, n FROM d ORDER BY day",
    "SELECT point FROM trustiot.tenant_telemetry_1m UNION ALL SELECT point FROM trustiot.tenant_telemetry_1h",
    "SELECT a.point FROM trustiot.tenant_telemetry_1m AS a LEFT JOIN trustiot.tenant_telemetry_1h AS b ON a.point = b.point AND a.device_id = b.device_id",
    "SELECT point FROM trustiot.tenant_telemetry WHERE device_id IN (SELECT device_id FROM trustiot.tenant_telemetry_1h) AND EXISTS (SELECT 1 FROM trustiot.tenant_telemetry_1m)",
    "SELECT DISTINCT point FROM trustiot.tenant_telemetry ORDER BY point DESC NULLS LAST LIMIT 5 OFFSET 2",
    "SELECT point, ts FROM trustiot.tenant_telemetry ORDER BY ts DESC LIMIT 1 BY point",
    "SELECT point, count() FROM trustiot.tenant_telemetry GROUP BY ROLLUP (point)",
    "SELECT count(DISTINCT point), uniqExact(device_id), countIf(value_float > 10) FROM trustiot.tenant_telemetry",
    "SELECT 1e3, 2.5E-1, true, NULL, 'a' = 'a' FROM trustiot.tenant_telemetry",
    "SELECT point /* a comment */ FROM trustiot.tenant_telemetry -- a comment to the end of the line\n WHERE value_float IS NOT NULL",
    "SELECT NOT value_bool = true, NOT (value_int > 1 AND value_int < 5) FROM trustiot.tenant_telemetry",
    "SELECT toDateTime('2026-10-01 00:00:00', 'Asia/Taipei'), formatDateTime(ts, '%F %T', 'UTC') FROM trustiot.tenant_telemetry",
    "SELECT if(value_bool, 1, 0), multiIf(value_int > 1, 'a', value_int > 0, 'b', 'c') FROM trustiot.tenant_telemetry",
    "SELECT * FROM (SELECT point, ts FROM trustiot.tenant_telemetry ORDER BY ts DESC LIMIT 5) AS t ORDER BY ts",
    "SELECT point FROM trustiot.tenant_telemetry WHERE point = 'a' OR point = 'b' AND value_int = 1",
    "SELECT ((value_float)) + (1) FROM trustiot.tenant_telemetry WHERE (((point = 'a')))",
    "SELECT point FROM trustiot.tenant_telemetry WHERE value_string IS NULL AND point != 'a' AND point <> 'b' AND value_int >= 1 AND value_int <= 9",
    "SELECT argMax(last.value_float, last.ts), avgIfOrNull(value_sum, readings > 0) FROM trustiot.tenant_telemetry_1m",
];

/// Queries whose operators sqlparser and ClickHouse 26.8 rank differently, and controls they
/// rank alike, each with sqlparser's grouping written out in parentheses: ClickHouse must read
/// the printed query as that grouping. The flag: ClickHouse reads the text as written another
/// way (the security review of 2026-10-26 on Laysi/trustiot-v2#143).
const PRECEDENCE: &[(&str, &str, bool)] = &[
    (
        "SELECT 1 + 2 || 3 FROM trustiot.tenant_telemetry",
        "SELECT 1 + (2 || 3) FROM trustiot.tenant_telemetry",
        true,
    ),
    (
        "SELECT 1 FROM trustiot.tenant_telemetry WHERE point = 'a' || 'b' + 1",
        "SELECT 1 FROM trustiot.tenant_telemetry WHERE point = (('a' || 'b') + 1)",
        true,
    ),
    (
        "SELECT 'a' LIKE 'a' = 0 FROM trustiot.tenant_telemetry",
        "SELECT 'a' LIKE ('a' = 0) FROM trustiot.tenant_telemetry",
        true,
    ),
    (
        "SELECT 1 BETWEEN 0 AND 2 = 0 FROM trustiot.tenant_telemetry",
        "SELECT (1 BETWEEN 0 AND 2) = 0 FROM trustiot.tenant_telemetry",
        true,
    ),
    (
        "SELECT 2 * 3 || 4, -1 || 2 FROM trustiot.tenant_telemetry",
        "SELECT (2 * 3) || 4, (-1) || 2 FROM trustiot.tenant_telemetry",
        false,
    ),
    (
        "SELECT 0 = NULL IS NULL, NOT 0 IS NULL, 1 = 1 IN (0), 1 IN (1) = 0, 1 < 2 = 1 FROM trustiot.tenant_telemetry",
        "SELECT (0 = NULL) IS NULL, NOT (0 IS NULL), (1 = 1) IN (0), (1 IN (1)) = 0, (1 < 2) = 1 FROM trustiot.tenant_telemetry",
        false,
    ),
    (
        "SELECT 'a' || 'b' LIKE 'ab', -2 * -3, - -2 FROM trustiot.tenant_telemetry",
        "SELECT ('a' || 'b') LIKE 'ab', (-2) * (-3), -(-2) FROM trustiot.tenant_telemetry",
        false,
    ),
];

/// A token as the assistant's token exchange makes it: `act` is the assistant.
fn assistant_token(issuer: &TestIssuer, tenant: TenantId, role: TenantRole) -> String {
    let mut claims = TestIssuer::claims(
        ServiceName::TelemetryQuery,
        Some(TenantGrant { id: tenant, role }),
    );
    claims.act = Some(Actor {
        sub: ServiceName::Assistant,
    });
    issuer.token(&claims)
}

/// Calls the endpoint as `token` (none: without one); the status and the body as JSON.
async fn run(
    service: &Service,
    token: Option<&str>,
    sql: &str,
    max_rows: Option<u32>,
) -> (u16, String) {
    let body = serde_json::to_string(&RunSql {
        sql: sql.to_owned(),
        max_rows,
    })
    .expect("a request");
    service.post(RunSql::ROUTE, token, &body).await
}

fn json(body: &str) -> Value {
    serde_json::from_str(body).unwrap_or_else(|error| panic!("{error}: {body}"))
}

/// What a refused query's 422 must be: `validation_failed` with the pure check's refusal as
/// `details`, when the check refuses it.
fn assert_refusal_body(sql: &str, body: &str, details: &Value) {
    let value = json(body);
    assert_eq!(
        value,
        json!({
            "code": "validation_failed",
            "message_key": "error.validation_failed",
            "details": details,
        }),
        "{sql}"
    );
    assert!(
        sql.len() < 8 || !body.contains(sql),
        "{sql}: the SQL echoed"
    );
    for text in ["DB::Exception", "Code:", "trustiot.telemetry"] {
        assert!(!body.contains(text), "{sql}: {text} in {body}");
    }
}

/// The pure check's refusal of `sql`, as the endpoint's details.
fn checked_refusal(sql: &str, max_rows: u32) -> Option<Value> {
    let sql = sql.to_owned();
    // The check's own stack: a deep query needs more than a test thread's.
    std::thread::Builder::new()
        .stack_size(telemetry_query::sql::STACK_BYTES)
        .spawn(move || match check(&sql, max_rows) {
            Err(CheckError::Refused(refusal)) => {
                Some(serde_json::to_value(refusal).expect("a refusal"))
            }
            _ => None,
        })
        .expect("a thread")
        .join()
        .expect("the check does not panic")
}

/// The text of an endpoint's answer that a marker could show in: the rows of a 200, the whole
/// body otherwise.
fn shown(status: u16, body: &str) -> String {
    if status == 200 {
        json(body)["rows"].to_string()
    } else {
        body.to_owned()
    }
}

/// The class of a 422, or the status.
fn outcome(status: u16, body: &str) -> String {
    match status {
        200 => format!(
            "answered, {} rows",
            json(body)["rows"].as_array().map_or(0, Vec::len)
        ),
        422 => format!(
            "refused: {}",
            json(body)["details"]["class"].as_str().unwrap_or("?")
        ),
        other => format!("status {other}"),
    }
}

/// Runs the period's `routes` as `me` through the endpoint: each is refused (422) or answers
/// without a marker outside the period or of another tenant, and, when it answers, with the
/// caller's markers the route shows when run directly.
#[allow(
    clippy::print_stderr,
    reason = "prints each route's outcome for the test report"
)]
async fn run_period_routes(
    service: &Service,
    token: &str,
    me: &Tenant,
    account: &str,
    routes: &[Route],
) {
    for route in routes {
        let sql = route
            .sql
            .replace("TENANT_SELF", &me.id)
            .replace("ACCOUNT_SELF", account)
            .replace("VICTIM_ID", period::VICTIM);
        let (status, body) = run(service, Some(token), &sql, Some(10_000)).await;
        assert!(
            status == 200 || status == 422,
            "{}: status {status}: {body}",
            route.name
        );
        let text = shown(status, &body);
        let leaked: Vec<String> = period::forbidden(me.letter, route.views)
            .into_iter()
            .filter(|marker| text.contains(marker.as_str()))
            .collect();
        assert!(
            leaked.is_empty(),
            "{}: readings outside the period or of another tenant: {leaked:?}\n{body}",
            route.name
        );
        if let (200, Outcome::Answers(ages)) = (status, route.outcome) {
            for age in ages {
                let wanted = period::marker(me.letter, age);
                assert!(
                    text.contains(&wanted),
                    "{}: {wanted} missing: {body}",
                    route.name
                );
            }
        }
        let url = if route.url.is_empty() {
            ""
        } else {
            " (its URL settings cannot be sent)"
        };
        eprintln!(
            "endpoint period {:>42}: {}{url}",
            route.name,
            outcome(status, &body)
        );
    }
}

/// Runs the PoC's attacks, system tables and heavy queries as A through the endpoint.
#[allow(
    clippy::print_stderr,
    reason = "prints each attack's outcome for the test report"
)]
async fn run_attacks(service: &Service, token: &str) {
    let sqls = ATTACKS
        .iter()
        .map(|attack| (attack.name, attack.sql, !attack.url.is_empty()))
        .chain(SYSTEM_TABLES.iter().map(|sql| (*sql, *sql, false)))
        .chain(HEAVY.iter().map(|(name, sql, _)| (*name, *sql, false)));
    for (name, sql, has_url) in sqls {
        let (status, body) = run(service, Some(token), sql, None).await;
        assert!(
            status == 200 || status == 422,
            "{name}: status {status}: {body}"
        );
        for marker in [VICTIM_MARKER, B_MARKER] {
            assert!(
                !body.contains(marker),
                "{name}: another tenant's reading: {body}"
            );
        }
        let url = if has_url {
            " (its URL settings cannot be sent)"
        } else {
            ""
        };
        eprintln!(
            "endpoint attack {name:>42}: {}{url}",
            outcome(status, &body)
        );
    }
}

/// Every allowed function exists in the pinned ClickHouse with the same case rule and kind, an
/// alias names an allowed function, and each combinator is one or two of ClickHouse's.
async fn functions_are_clickhouses(stack: &Stack) {
    let rows = stack
        .clickhouse
        .admin_ok(
            "SELECT name, is_aggregate, case_insensitive, alias_to FROM system.functions FORMAT TSVRaw",
        )
        .await;
    let known: BTreeMap<&str, (bool, bool, &str)> = rows
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            Some((
                fields.next()?,
                (fields.next()? == "1", fields.next()? == "1", fields.next()?),
            ))
        })
        .collect();
    let allowed: BTreeSet<&str> = FUNCTIONS.iter().map(|function| function.name).collect();
    let mut wrong = Vec::new();
    for function in FUNCTIONS {
        let Some((aggregate, case_insensitive, alias_to)) = known.get(function.name) else {
            wrong.push(format!(
                "{} is not a function of the pinned ClickHouse",
                function.name
            ));
            continue;
        };
        if *case_insensitive != function.case_insensitive {
            wrong.push(format!(
                "{}: case_insensitive is {case_insensitive}",
                function.name
            ));
        }
        if *aggregate != matches!(function.kind, Kind::Aggregate | Kind::Window) {
            wrong.push(format!("{}: is_aggregate is {aggregate}", function.name));
        }
        if !alias_to.is_empty() && !allowed.contains(alias_to) {
            wrong.push(format!(
                "{} is an alias of {alias_to}, which is not allowed",
                function.name
            ));
        }
    }
    assert!(
        wrong.is_empty(),
        "the allowlist differs from ClickHouse:\n{}",
        wrong.join("\n")
    );
    let combinators = stack
        .clickhouse
        .admin_ok("SELECT name FROM system.aggregate_function_combinators FORMAT TSVRaw")
        .await;
    let combinators: BTreeSet<&str> = combinators.lines().collect();
    for combinator in COMBINATORS {
        let split = (1..combinator.len()).any(|at| {
            combinator
                .split_at_checked(at)
                .is_some_and(|(first, second)| {
                    combinators.contains(first) && combinators.contains(second)
                })
        });
        assert!(
            combinators.contains(combinator) || split,
            "{combinator} is not one or two of ClickHouse's combinators"
        );
    }
}

/// The check's canonical text of `sql`, which must pass.
fn canonical(sql: &str) -> String {
    let sql = sql.to_owned();
    std::thread::Builder::new()
        .stack_size(telemetry_query::sql::STACK_BYTES)
        .spawn(move || check(&sql, 200))
        .expect("a thread")
        .join()
        .expect("the check does not panic")
        .unwrap_or_else(|error| panic!("{error:?}"))
        .canonical
}

/// `EXPLAIN AST` of `sql` as the admin.
async fn ast(stack: &Stack, sql: &str) -> String {
    stack
        .clickhouse
        .admin(&format!("EXPLAIN AST {sql}"))
        .await
        .unwrap_or_else(|error| panic!("EXPLAIN AST {sql}\n{}", error.body))
}

#[allow(
    clippy::print_stderr,
    reason = "prints the outcomes for the test report"
)]
#[allow(
    clippy::too_many_lines,
    reason = "one test in order: the binary shares one product database"
)]
#[tokio::test(flavor = "multi_thread")]
async fn the_endpoint_runs_only_checked_selects_as_the_callers_tenant() {
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
    let c = stack.create_tenant(&issuer, "Plant C").await;
    for tenant in [a, b, c] {
        let token = issuer.tenant_token(ServiceName::TelemetryQuery, tenant, TenantRole::Viewer);
        wait_account_ready(&service, &token, tenant).await;
    }
    let token_a = assistant_token(&issuer, a, TenantRole::Viewer);
    let token_b = assistant_token(&issuer, b, TenantRole::Viewer);
    let token_c = assistant_token(&issuer, c, TenantRole::Viewer);

    // Readings: C's temperatures; A's, B's and the PoC victim's markers; and the period's
    // readings of A, B and the period's victim at every age.
    let device = uuid::Uuid::new_v4();
    let now_ms = Utc::now().timestamp_millis();
    stack
        .insert(&Readings {
            tenant: c,
            device,
            point: "temperature",
            kind: "float",
            start_ms: now_ms - 2 * 86_400_000,
            step_ms: 3_600_000,
            count: TEMPERATURES,
            marker: "",
        })
        .await;
    for (tenant, marker) in [
        (a, A_MARKER),
        (b, B_MARKER),
        (VICTIM.parse::<TenantId>().expect("an ID"), VICTIM_MARKER),
    ] {
        stack
            .insert(&Readings {
                tenant,
                device,
                point: "poc",
                kind: "string",
                start_ms: now_ms - 3_600_000,
                step_ms: 60_000,
                count: 10,
                marker,
            })
            .await;
    }
    let period_device = uuid::Uuid::new_v4().to_string();
    let tenant_a = Tenant {
        letter: 'A',
        id: a.to_string(),
    };
    let tenant_b = Tenant {
        letter: 'B',
        id: b.to_string(),
    };
    for (letter, id) in [
        ('A', a.to_string()),
        ('B', b.to_string()),
        ('V', period::VICTIM.to_owned()),
    ] {
        for (age, seconds) in AGES {
            period::insert(
                &stack,
                &Aged {
                    tenant: &id,
                    device: &period_device,
                    point: "status",
                    age_s: seconds,
                    step_ms: 1,
                    count: 3,
                    marker: &period::marker(letter, age),
                },
            )
            .await;
        }
    }

    // 1. A model's query: the answer, the checked SQL, the columns, the period.
    let sql = "SELECT point, count() AS readings, round(avg(value_float), 3) AS mean
               FROM tenant_telemetry WHERE point = 'temperature' GROUP BY point";
    let before: DateTime<Utc> = Utc::now();
    let (status, body) = run(&service, Some(&token_c), sql, None).await;
    let after: DateTime<Utc> = Utc::now();
    assert_eq!(status, 200, "{body}");
    let answer = json(&body);
    assert_eq!(
        answer["sql"],
        "SELECT point, count() AS readings, round(avg(value_float), 3) AS mean FROM trustiot.tenant_telemetry WHERE point = 'temperature' GROUP BY point"
    );
    assert_eq!(
        answer["columns"],
        json!([
            {"name": "point", "type": "LowCardinality(String)"},
            {"name": "readings", "type": "UInt64"},
            {"name": "mean", "type": "Float64"},
        ])
    );
    assert_eq!(answer["rows"][0][0], "temperature");
    assert_eq!(answer["rows"][0][1], TEMPERATURES);
    let queried_at: DateTime<Utc> = answer["queried_at"]
        .as_str()
        .and_then(|text| text.parse().ok())
        .expect("queried_at");
    assert!(before <= queried_at && queried_at <= after, "{body}");
    for (field, view) in [
        ("tenant_telemetry", View::Raw),
        ("tenant_telemetry_1m", View::Minute),
        ("tenant_telemetry_1h", View::Hour),
    ] {
        let from: DateTime<Utc> = answer["queryable_from"][field]
            .as_str()
            .and_then(|text| text.parse().ok())
            .unwrap_or_else(|| panic!("queryable_from.{field}: {body}"));
        assert!(
            EXPECTED_WINDOWS.earliest(view, before) <= from
                && from <= EXPECTED_WINDOWS.earliest(view, after),
            "queryable_from.{field}: {from}"
        );
    }
    eprintln!("endpoint model query: {body}");
    // Every tenant role may run SQL through the assistant; nothing else may.
    for role in [TenantRole::Editor, TenantRole::Admin] {
        let (status, other) = run(
            &service,
            Some(&assistant_token(&issuer, c, role)),
            sql,
            None,
        )
        .await;
        assert_eq!(status, 200, "{role:?}: {other}");
        assert_eq!(json(&other)["rows"], answer["rows"], "{role:?}");
    }
    let user_c = issuer.tenant_token(ServiceName::TelemetryQuery, c, TenantRole::Admin);
    let mut other_service = TestIssuer::claims(
        ServiceName::TelemetryQuery,
        Some(TenantGrant {
            id: c,
            role: TenantRole::Admin,
        }),
    );
    other_service.act = Some(Actor {
        sub: ServiceName::Assets,
    });
    let mut no_tenant = TestIssuer::claims(ServiceName::TelemetryQuery, None);
    no_tenant.act = Some(Actor {
        sub: ServiceName::Assistant,
    });
    for (name, token, expected) in [
        ("a user's own token", Some(user_c), 403),
        (
            "another service's exchange",
            Some(issuer.token(&other_service)),
            403,
        ),
        (
            "the assistant without a tenant",
            Some(issuer.token(&no_tenant)),
            403,
        ),
        ("no token", None, 401),
    ] {
        let (status, body) = run(&service, token.as_deref(), sql, None).await;
        assert_eq!(status, expected, "{name}: {body}");
        assert!(!body.contains("temperature"), "{name}: {body}");
        eprintln!("endpoint caller {name:>42}: status {status}");
    }
    // Bodies that are not a request, and one beyond the body limit.
    for (name, body, expected) in [
        ("not JSON", "SELECT 1".to_owned(), 400),
        ("no sql", "{\"max_rows\":5}".to_owned(), 400),
        (
            "max_rows 0",
            json!({"sql": sql, "max_rows": 0}).to_string(),
            400,
        ),
        (
            "max_rows 10001",
            json!({"sql": sql, "max_rows": 10_001}).to_string(),
            400,
        ),
        (
            "a body over 64 KiB",
            json!({"sql": format!("SELECT '{}' FROM tenant_telemetry", "x".repeat(70_000))})
                .to_string(),
            422,
        ),
    ] {
        let (status, answer) = service.post(RunSql::ROUTE, Some(&token_c), &body).await;
        assert_eq!(status, expected, "{name}: {answer}");
        if expected == 422 {
            assert_eq!(
                json(&answer)["details"],
                json!({"class": "too_long", "max_bytes": MAX_SQL_BYTES})
            );
        }
        eprintln!("endpoint body {name:>42}: status {status}");
    }

    // A model's queries all run on ClickHouse.
    let queries: Vec<&str> = include_str!("data/model-queries.sql")
        .split("\n%%\n")
        .map(str::trim)
        .filter(|query| !query.is_empty() && !query.starts_with("-- Queries a model"))
        .collect();
    for sql in &queries {
        let (status, body) = run(&service, Some(&token_c), sql, Some(10_000)).await;
        assert_eq!(status, 200, "{sql}\n{body}");
    }
    eprintln!("endpoint: {} model queries answered", queries.len());

    // 2. Every refusal class, with its fixed details only.
    let chain = format!("SELECT {}1 FROM tenant_telemetry", "1+".repeat(3_000));
    let long = format!("SELECT '{}' FROM tenant_telemetry", "x".repeat(17 * 1024));
    let parse_stage: &[(&str, &str)] = &[
        ("too_long", &long),
        ("too_complex", &chain),
        ("syntax", "SELECT point\nFROM tenant_telemetry WHERE AND 1"),
        (
            "character_not_allowed",
            "SELECT point FROM tenant_telemetry WHERE point = 'a\\b'",
        ),
        (
            "literal_not_allowed",
            "SELECT \"point\" FROM tenant_telemetry",
        ),
        ("not_a_select", "SET ROLE NONE"),
        (
            "multiple_statements",
            "SELECT 1 FROM tenant_telemetry; SELECT 2",
        ),
        ("table_not_allowed", "SELECT * FROM system.users"),
        (
            "function_not_allowed",
            "SELECT dictGet('d', 'a', 1) FROM tenant_telemetry",
        ),
        (
            "clause_not_allowed",
            "SELECT point FROM tenant_telemetry SETTINGS max_threads = 1",
        ),
        ("no_tenant_view", "SELECT 1"),
        (
            "time_zone_required",
            "SELECT toStartOfHour(ts) FROM tenant_telemetry",
        ),
    ];
    for (class, sql) in parse_stage {
        let (status, body) = run(&service, Some(&token_c), sql, None).await;
        assert_eq!(status, 422, "{class}: {body}");
        let details = checked_refusal(sql, 200).expect("the check refuses it");
        assert_eq!(details["class"], *class, "{sql}");
        assert_refusal_body(sql, &body, &details);
        eprintln!("endpoint class {class:>32}: {details}");
    }
    // ClickHouse refuses ORDER BY or LIMIT after a query in parentheses: the check refuses it
    // with the class that names the way the model can write it, which runs.
    let views =
        "SELECT point FROM tenant_telemetry_1m UNION ALL SELECT point FROM tenant_telemetry_1h";
    let parenthesized = format!("({views}) ORDER BY point LIMIT 3");
    let (status, body) = run(&service, Some(&token_c), &parenthesized, None).await;
    assert_eq!(status, 422, "{parenthesized}: {body}");
    assert_refusal_body(
        &parenthesized,
        &body,
        &json!({"class": "clause_not_allowed", "clause": "set_operation_order"}),
    );
    let subquery = format!("SELECT * FROM ({views}) ORDER BY point LIMIT 3");
    let (status, body) = run(&service, Some(&token_c), &subquery, None).await;
    assert_eq!(status, 200, "{subquery}: {body}");
    eprintln!(
        "endpoint parenthesized query with ORDER BY: 422 set_operation_order; as a subquery: 200"
    );
    let temperatures = "SELECT value_float FROM tenant_telemetry WHERE point = 'temperature'";
    let execution_stage: &[(&str, Option<u32>, Value)] = &[
        (
            temperatures,
            Some(5),
            json!({"class": "too_many_rows", "max": 5}),
        ),
        (
            "SELECT leftPad('', 1000000, 'x') FROM tenant_telemetry WHERE point = 'temperature'",
            None,
            json!({"class": "result_too_large", "max_bytes": MAX_RESULT_BYTES}),
        ),
        (
            "SELECT length(leftPad('', 600000000, 'x')) FROM tenant_telemetry WHERE point = 'temperature'",
            None,
            json!({"class": "query_too_heavy"}),
        ),
        (
            "SELECT nothing_here FROM tenant_telemetry",
            None,
            json!({"class": "query_error", "kind": "unknown_column"}),
        ),
        (
            "SELECT value_float + 'x' FROM tenant_telemetry",
            None,
            json!({"class": "query_error", "kind": "type_mismatch"}),
        ),
        (
            "SELECT round(value_float, 1, 2) FROM tenant_telemetry",
            None,
            json!({"class": "query_error", "kind": "bad_arguments"}),
        ),
        (
            "SELECT toInt64(point) FROM tenant_telemetry WHERE point = 'temperature'",
            None,
            json!({"class": "query_error", "kind": "cannot_parse_value"}),
        ),
        (
            "SELECT point, count() FROM tenant_telemetry",
            None,
            json!({"class": "query_error", "kind": "aggregate_misuse"}),
        ),
        (
            "SELECT intDiv(value_int, 0) FROM tenant_telemetry WHERE point = 'temperature'",
            None,
            json!({"class": "query_error", "kind": "division_by_zero"}),
        ),
    ];
    for (sql, max_rows, details) in execution_stage {
        let (status, body) = run(&service, Some(&token_c), sql, *max_rows).await;
        assert_eq!(status, 422, "{sql}: {body}");
        assert_refusal_body(sql, &body, details);
        eprintln!(
            "endpoint class {:>32}: {details}",
            details["class"].as_str().unwrap_or("?")
        );
    }

    // 3. The PoC's attacks, as A.
    run_attacks(&service, &token_a).await;

    // 4. The period's routes: as A, then settings and evaluation order as B.
    let (account_a, account_b) = (stack.tenant_account(a), stack.tenant_account(b));
    run_period_routes(&service, &token_a, &tenant_a, &account_a.user, ROUTES).await;
    run_period_routes(
        &service,
        &token_b,
        &tenant_b,
        &account_b.user,
        SETTINGS_ROUTES,
    )
    .await;
    run_period_routes(
        &service,
        &token_b,
        &tenant_b,
        &account_b.user,
        EVALUATION_ROUTES,
    )
    .await;

    // 5. The same SQL as A and as B: each its own readings.
    let same = "SELECT DISTINCT value_string FROM tenant_telemetry WHERE point = 'poc' ORDER BY value_string";
    for (token, own, others) in [
        (&token_a, A_MARKER, [B_MARKER, VICTIM_MARKER]),
        (&token_b, B_MARKER, [A_MARKER, VICTIM_MARKER]),
    ] {
        let (status, body) = run(&service, Some(token), same, None).await;
        assert_eq!(status, 200, "{body}");
        let rows = json(&body)["rows"].clone();
        let rows = rows.as_array().expect("rows");
        assert_eq!(rows.len(), 10, "{body}");
        for row in rows {
            let text = row[0].as_str().expect("a text reading");
            assert!(text.starts_with(own), "{text}");
            for other in others {
                assert!(!text.contains(other), "{text}");
            }
        }
        eprintln!("endpoint same SQL: {} rows, all {own}", rows.len());
    }

    // 6. Statements that are not one SELECT never reach ClickHouse.
    for sql in NOT_SENT {
        let (status, body) = run(&service, Some(&token_a), sql, None).await;
        assert_eq!(status, 422, "{sql}: {body}");
        eprintln!(
            "endpoint not sent {:>60}: {}",
            sql.replace('\n', " "),
            outcome(status, &body)
        );
    }
    stack.clickhouse.admin_ok("SYSTEM FLUSH LOGS").await;
    let users = [&account_a, &account_b, &stack.tenant_account(c)]
        .map(|account: &Account| format!("'{}'", account.user))
        .join(", ");
    let kinds = stack
        .clickhouse
        .admin_ok(&format!(
            "SELECT query_kind, count() FROM system.query_log
             WHERE user IN ({users}) AND type = 'QueryStart' GROUP BY query_kind
             ORDER BY query_kind FORMAT TSVRaw"
        ))
        .await;
    let kinds: Vec<&str> = kinds.lines().collect();
    assert!(
        kinds.len() == 1 && kinds[0].starts_with("Select\t"),
        "the tenant accounts ran only SELECTs: {kinds:?}"
    );
    let probes = stack
        .clickhouse
        .admin_ok(&format!(
            "SELECT count() FROM system.query_log
             WHERE user IN ({users}) AND position(query, '{PROBE}') > 0 FORMAT TSVRaw"
        ))
        .await;
    assert_eq!(probes.trim(), "0", "a refused statement reached ClickHouse");
    eprintln!("endpoint query_log: tenant accounts ran {kinds:?}; refused statements in it: 0");

    // 7. ClickHouse parses what the check prints as it parses what was written, groups the
    //    operators as sqlparser did, and knows every allowed function.
    for sql in AS_WRITTEN {
        let canonical = canonical(sql);
        assert_eq!(
            ast(&stack, sql).await,
            ast(&stack, &canonical).await,
            "{sql}\n{canonical}"
        );
    }
    eprintln!(
        "endpoint: {} queries parse alike as written and as printed",
        AS_WRITTEN.len()
    );
    for (written, grouped, differs) in PRECEDENCE {
        let canonical = canonical(written);
        let printed = ast(&stack, &canonical).await;
        assert_eq!(
            printed,
            ast(&stack, grouped).await,
            "ClickHouse groups the printed query as sqlparser did: {written}\n{canonical}"
        );
        assert_eq!(
            ast(&stack, written).await != printed,
            *differs,
            "{written}: whether ClickHouse groups the text as written otherwise"
        );
        eprintln!(
            "endpoint grouping {written:>58} -> {canonical}{}",
            if *differs {
                " (ClickHouse groups the text as written otherwise)"
            } else {
                ""
            }
        );
    }
    functions_are_clickhouses(&stack).await;
    eprintln!(
        "endpoint: {} allowed functions and {} combinators are the pinned ClickHouse's",
        FUNCTIONS.len(),
        COMBINATORS.len()
    );
}
