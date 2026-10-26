//! The check end to end: what passes and how it prints, what is refused and why. Statements
//! other than one SELECT, schema changes, and long or deep queries the test builds are tested in
//! `tests/sql_check.rs`: repo-check and sql-check keep such text out of `src`.

use contract::api::telemetry_query::{SqlClause, SqlRefusal};

use super::*;

/// Runs `step` on a thread with the check's stack, as the service does.
fn on_thread<T: Send + 'static>(step: impl FnOnce() -> T + Send + 'static) -> T {
    thread::Builder::new()
        .stack_size(STACK_BYTES)
        .spawn(step)
        .expect("a thread")
        .join()
        .expect("the check does not panic")
}

fn run(sql: &str, max_rows: u32) -> Result<Checked, CheckError> {
    let sql = sql.to_owned();
    on_thread(move || check(&sql, max_rows))
}

fn passes(sql: &str) -> Checked {
    run(sql, 200).unwrap_or_else(|error| panic!("{sql}: {error:?}"))
}

fn refusal(sql: &str) -> SqlRefusal {
    match run(sql, 200) {
        Err(CheckError::Refused(refusal)) => refusal,
        other => panic!("{sql}: expected a refusal, got {other:?}"),
    }
}

fn clause(clause: SqlClause) -> SqlRefusal {
    SqlRefusal::ClauseNotAllowed { clause }
}

fn table(name: &str) -> SqlRefusal {
    SqlRefusal::TableNotAllowed {
        table: name.to_owned(),
    }
}

fn function(name: &str) -> SqlRefusal {
    SqlRefusal::FunctionNotAllowed {
        function: name.to_owned(),
    }
}

/// Queries a model writes over the views, which pass: `tests/data/model-queries.sql`, which
/// `tests/sql_api.rs` also runs on ClickHouse.
fn model_queries() -> Vec<&'static str> {
    include_str!("../../tests/data/model-queries.sql")
        .split("\n%%\n")
        .map(str::trim)
        .filter(|query| !query.is_empty() && !query.starts_with("-- Queries a model"))
        .collect()
}

#[test]
fn model_queries_pass_and_print_their_canonical_form() {
    let queries = model_queries();
    assert!(queries.len() >= 40, "the corpus: {}", queries.len());
    for sql in queries {
        let checked = passes(sql);
        // Idempotent: the canonical text passes again and prints the same.
        assert_eq!(
            passes(&checked.canonical).canonical,
            checked.canonical,
            "{sql}"
        );
        assert!(
            checked
                .statement
                .ends_with(" FORMAT JSONCompactEachRowWithNamesAndTypes"),
            "{sql}: {}",
            checked.statement
        );
        assert!(!checked.canonical.contains("--"), "{sql}");
    }
}

#[test]
fn the_tree_is_rewritten_into_the_form_clickhouse_runs() {
    for (sql, canonical) in [
        (
            "select COUNT(*) from tenant_telemetry -- comment\n/* another */",
            "SELECT count(*) FROM trustiot.tenant_telemetry",
        ),
        (
            "SELECT CAST(value_int AS Float64), value_int::String FROM tenant_telemetry t",
            "SELECT toFloat64(value_int), toString(value_int) FROM trustiot.tenant_telemetry AS t",
        ),
        (
            "SELECT FLOOR(value_float), CEIL(value_float, 2), SUBSTRING(point, 1, 3) FROM tenant_telemetry",
            "SELECT floor(value_float), ceil(value_float, 2), substring(point, 1, 3) FROM trustiot.tenant_telemetry",
        ),
        (
            "SELECT SUMIf(value_float, 1), Avg(value_float), Corr(value_float, value_int) FROM tenant_telemetry",
            "SELECT sumIf(value_float, 1), avg(value_float), corr(value_float, value_int) FROM trustiot.tenant_telemetry",
        ),
        (
            "SELECT a.point FROM tenant_telemetry_1m a JOIN tenant_telemetry_1h b ON a.point = b.point",
            "SELECT a.point FROM trustiot.tenant_telemetry_1m AS a JOIN trustiot.tenant_telemetry_1h AS b ON a.point = b.point",
        ),
        (
            "WITH x AS (SELECT point FROM tenant_telemetry) SELECT * FROM x",
            "WITH x AS (SELECT point FROM trustiot.tenant_telemetry) SELECT * FROM x",
        ),
        // Printed as `--1`, the rest of the line would be a comment to ClickHouse.
        (
            "SELECT - -1, - - - value_int, 1 - -1 FROM tenant_telemetry",
            "SELECT -(-1), -(-(-value_int)), 1 - (-1) FROM trustiot.tenant_telemetry",
        ),
    ] {
        assert_eq!(passes(sql).canonical, canonical, "{sql}");
    }
}

/// sqlparser and ClickHouse 26.8 rank some operators differently, so the printed text carries
/// sqlparser's grouping in parentheses (`tests/sql_api.rs` shows ClickHouse reading it so).
#[test]
fn every_operand_that_is_an_operation_is_printed_in_parentheses() {
    for (sql, canonical) in [
        // `||` above `+` in sqlparser, below it in ClickHouse.
        (
            "SELECT 1 + 2 || 3 FROM tenant_telemetry",
            "SELECT 1 + (2 || 3) FROM trustiot.tenant_telemetry",
        ),
        (
            "SELECT point FROM tenant_telemetry WHERE point = 'a' || 'b' + 1",
            "SELECT point FROM trustiot.tenant_telemetry WHERE point = (('a' || 'b') + 1)",
        ),
        // `LIKE` below `=` in sqlparser, at its rank in ClickHouse; `BETWEEN … AND` takes `2 = 0`
        // as its upper bound in ClickHouse only.
        (
            "SELECT 'a' LIKE 'a' = 0 FROM tenant_telemetry",
            "SELECT 'a' LIKE ('a' = 0) FROM trustiot.tenant_telemetry",
        ),
        (
            "SELECT 1 BETWEEN 0 AND 2 = 0 FROM tenant_telemetry",
            "SELECT (1 BETWEEN 0 AND 2) = 0 FROM trustiot.tenant_telemetry",
        ),
        (
            "SELECT point FROM tenant_telemetry WHERE value_int + 1 IS NULL OR NOT value_bool = true",
            "SELECT point FROM trustiot.tenant_telemetry WHERE ((value_int + 1) IS NULL) OR (NOT (value_bool = true))",
        ),
        (
            "SELECT point FROM tenant_telemetry WHERE value_int * 2 IN (2, 4) AND point || 'x' NOT IN (SELECT point FROM tenant_telemetry_1m)",
            "SELECT point FROM trustiot.tenant_telemetry WHERE ((value_int * 2) IN (2, 4)) AND ((point || 'x') NOT IN (SELECT point FROM trustiot.tenant_telemetry_1m))",
        ),
        (
            "SELECT [1, 2][1] + 1, now() - INTERVAL 1 DAY, sum(value_int) OVER () * 2 FROM tenant_telemetry",
            "SELECT ([1, 2][1]) + 1, now() - (INTERVAL 1 DAY), (sum(value_int) OVER ()) * 2 FROM trustiot.tenant_telemetry",
        ),
        // A chain of one rank stays flat: both parsers group it from the left.
        (
            "SELECT 1 + 2 - 3 + 4, 2 * 3 / 4 % 5, 'a' || 'b' || 'c' FROM tenant_telemetry WHERE 1 AND 2 AND 3 OR 4 OR 5",
            "SELECT 1 + 2 - 3 + 4, 2 * 3 / 4 % 5, 'a' || 'b' || 'c' FROM trustiot.tenant_telemetry WHERE (1 AND 2 AND 3) OR 4 OR 5",
        ),
        // Of different ranks, each operand in parentheses.
        (
            "SELECT 1 + 2 * 3, 1 * 2 + 3, 1 - (2 - 3), 2 * 3 || 'x' FROM tenant_telemetry",
            "SELECT 1 + (2 * 3), (1 * 2) + 3, 1 - (2 - 3), (2 * 3) || 'x' FROM trustiot.tenant_telemetry",
        ),
    ] {
        let checked = passes(sql);
        assert_eq!(checked.canonical, canonical, "{sql}");
        assert_eq!(passes(canonical).canonical, canonical, "{sql}: idempotent");
    }
}

#[test]
fn the_row_limit_is_one_more_than_allowed_and_never_raises_the_queries_own() {
    let limit = |sql: &str, max_rows: u32| {
        run(sql, max_rows)
            .unwrap_or_else(|error| panic!("{sql}: {error:?}"))
            .statement
    };
    assert_eq!(
        limit("SELECT point FROM tenant_telemetry", 200),
        "SELECT point FROM trustiot.tenant_telemetry LIMIT 201 FORMAT JSONCompactEachRowWithNamesAndTypes"
    );
    assert_eq!(
        limit("SELECT point FROM tenant_telemetry LIMIT 10", 200),
        "SELECT point FROM trustiot.tenant_telemetry LIMIT 10 FORMAT JSONCompactEachRowWithNamesAndTypes"
    );
    let checked = run(
        "SELECT point FROM tenant_telemetry LIMIT 5000 OFFSET 7",
        10_000,
    )
    .expect("passes");
    assert_eq!(
        checked.canonical,
        "SELECT point FROM trustiot.tenant_telemetry LIMIT 5000 OFFSET 7"
    );
    assert_eq!(
        limit(
            "SELECT point FROM tenant_telemetry LIMIT 50000 OFFSET 7",
            10_000
        ),
        "SELECT point FROM trustiot.tenant_telemetry LIMIT 10001 OFFSET 7 FORMAT JSONCompactEachRowWithNamesAndTypes"
    );
    assert_eq!(
        limit("SELECT point FROM tenant_telemetry LIMIT 5, 1000", 200),
        "SELECT point FROM trustiot.tenant_telemetry LIMIT 201 OFFSET 5 FORMAT JSONCompactEachRowWithNamesAndTypes"
    );
    assert_eq!(
        limit(
            "SELECT point FROM tenant_telemetry LIMIT 99999999999999999999999",
            200
        ),
        "SELECT point FROM trustiot.tenant_telemetry LIMIT 201 FORMAT JSONCompactEachRowWithNamesAndTypes"
    );
    // A set operation and LIMIT BY are wrapped: their own LIMIT means something else.
    assert_eq!(
        limit(
            "SELECT point FROM tenant_telemetry_1m UNION ALL SELECT point FROM tenant_telemetry_1h",
            200
        ),
        "SELECT * FROM (SELECT point FROM trustiot.tenant_telemetry_1m UNION ALL SELECT point FROM trustiot.tenant_telemetry_1h) LIMIT 201 FORMAT JSONCompactEachRowWithNamesAndTypes"
    );
    assert_eq!(
        limit(
            "SELECT point FROM tenant_telemetry ORDER BY ts LIMIT 1 BY point",
            1
        ),
        "SELECT * FROM (SELECT point FROM trustiot.tenant_telemetry ORDER BY ts LIMIT 1 BY point) LIMIT 2 FORMAT JSONCompactEachRowWithNamesAndTypes"
    );
}

#[test]
fn only_the_tenant_views_and_common_table_expressions_are_read() {
    for (sql, refused) in [
        (
            "SELECT * FROM trustiot.telemetry",
            table("trustiot.telemetry"),
        ),
        ("SELECT * FROM telemetry", table("telemetry")),
        (
            "SELECT * FROM trustiot.telemetry_1m_cells",
            table("trustiot.telemetry_1m_cells"),
        ),
        ("SELECT * FROM system.users", table("system.users")),
        ("SELECT * FROM system.one", table("system.one")),
        ("SELECT * FROM Tenant_Telemetry", table("Tenant_Telemetry")),
        (
            "SELECT * FROM TRUSTIOT.tenant_telemetry",
            table("TRUSTIOT.tenant_telemetry"),
        ),
        (
            "SELECT * FROM other.tenant_telemetry",
            table("other.tenant_telemetry"),
        ),
        (
            "SELECT * FROM a.trustiot.tenant_telemetry",
            table("a.trustiot.tenant_telemetry"),
        ),
        ("SELECT * FROM numbers(10)", table("numbers")),
        (
            "SELECT * FROM remote('h', trustiot.telemetry)",
            table("remote"),
        ),
        ("SELECT * FROM merge('trustiot', '.*')", table("merge")),
        ("SELECT * FROM url('http://x', CSV)", table("url")),
        ("SELECT * FROM file('x')", table("file")),
        (
            "SELECT * FROM clusterAllReplicas('c', trustiot.telemetry)",
            table("clusterAllReplicas"),
        ),
        (
            "WITH tenant_telemetry AS (SELECT * FROM trustiot.telemetry) SELECT * FROM tenant_telemetry",
            table("tenant_telemetry"),
        ),
        (
            "WITH TRUSTIOT AS (SELECT 1) SELECT * FROM tenant_telemetry",
            table("TRUSTIOT"),
        ),
        (
            "WITH a AS (SELECT 1), a AS (SELECT 2) SELECT * FROM tenant_telemetry",
            table("a"),
        ),
        // A common table expression sees the ones before it, not itself or later ones.
        ("WITH a AS (SELECT * FROM a) SELECT * FROM a", table("a")),
        (
            "WITH a AS (SELECT * FROM b), b AS (SELECT 1) SELECT * FROM tenant_telemetry",
            table("b"),
        ),
        // Not out of the subquery that defines it.
        (
            "SELECT * FROM (WITH a AS (SELECT 1) SELECT * FROM tenant_telemetry), a",
            table("a"),
        ),
        ("SELECT 1", SqlRefusal::NoTenantView),
        (
            "WITH a AS (SELECT 1) SELECT * FROM a",
            SqlRefusal::NoTenantView,
        ),
    ] {
        assert_eq!(refusal(sql), refused, "{sql}");
    }
}

#[test]
fn clauses_that_change_what_is_read_or_how_are_refused() {
    for (sql, refused) in [
        (
            "SELECT * FROM tenant_telemetry SETTINGS max_threads = 1",
            clause(SqlClause::Settings),
        ),
        (
            "SELECT * FROM (SELECT * FROM tenant_telemetry SETTINGS additional_table_filters = 1)",
            clause(SqlClause::Settings),
        ),
        (
            "SELECT * FROM tenant_telemetry FORMAT TSV",
            clause(SqlClause::Format),
        ),
        (
            "SELECT * FROM tenant_telemetry FINAL",
            clause(SqlClause::KeywordAlias),
        ),
        (
            "SELECT * FROM tenant_telemetry SAMPLE 0.1",
            clause(SqlClause::Sample),
        ),
        (
            "SELECT * FROM tenant_telemetry PREWHERE 1",
            clause(SqlClause::Prewhere),
        ),
        (
            "SELECT point, count() FROM tenant_telemetry GROUP BY point WITH TOTALS",
            clause(SqlClause::WithTotals),
        ),
        (
            "SELECT ts FROM tenant_telemetry ORDER BY ts WITH FILL STEP 1",
            clause(SqlClause::WithFill),
        ),
        (
            "SELECT DISTINCT ON (point) point FROM tenant_telemetry",
            clause(SqlClause::DistinctOn),
        ),
        // At the start, the first-word rule refuses it (NotASelect); inside, the walk does.
        (
            "SELECT * FROM (FROM tenant_telemetry SELECT point)",
            clause(SqlClause::FromFirst),
        ),
        (
            "WITH RECURSIVE a AS (SELECT 1) SELECT * FROM tenant_telemetry",
            clause(SqlClause::RecursiveCte),
        ),
        (
            "WITH a (x) AS (SELECT 1) SELECT * FROM tenant_telemetry",
            clause(SqlClause::CteForm),
        ),
        (
            "SELECT * FROM tenant_telemetry_1m ANY LEFT JOIN tenant_telemetry_1h AS b USING (point)",
            clause(SqlClause::KeywordAlias),
        ),
        (
            "SELECT * FROM tenant_telemetry_1m PASTE JOIN tenant_telemetry_1h",
            clause(SqlClause::KeywordAlias),
        ),
        (
            "SELECT * FROM tenant_telemetry_1m PASTE JOIN tenant_telemetry_1h ON 1 = 1",
            clause(SqlClause::KeywordAlias),
        ),
        (
            "SELECT * FROM tenant_telemetry_1m data, tenant_telemetry_1h",
            clause(SqlClause::KeywordAlias),
        ),
        (
            "SELECT * FROM tenant_telemetry_1m AS a GLOBAL JOIN tenant_telemetry_1h AS b USING (point)",
            clause(SqlClause::JoinKind),
        ),
        (
            "SELECT * FROM tenant_telemetry_1m AS a NATURAL JOIN tenant_telemetry_1h AS b",
            clause(SqlClause::JoinKind),
        ),
        (
            "SELECT * FROM tenant_telemetry_1m AS a JOIN tenant_telemetry_1h AS b",
            clause(SqlClause::JoinWithoutCondition),
        ),
        (
            "SELECT point FROM tenant_telemetry_1m UNION ALL SELECT point FROM tenant_telemetry_1h ORDER BY point",
            clause(SqlClause::SetOperationOrder),
        ),
        (
            "SELECT point FROM tenant_telemetry_1m UNION ALL SELECT point FROM tenant_telemetry_1h LIMIT 1",
            clause(SqlClause::SetOperationOrder),
        ),
        // ClickHouse refuses ORDER BY or LIMIT after a query in parentheses (the security
        // review of 2026-10-26 found the first one passing the check).
        (
            "(SELECT point FROM tenant_telemetry_1m UNION ALL SELECT point FROM tenant_telemetry_1h) ORDER BY point LIMIT 3",
            clause(SqlClause::SetOperationOrder),
        ),
        (
            "((SELECT point FROM tenant_telemetry_1m UNION ALL SELECT point FROM tenant_telemetry_1h)) LIMIT 3",
            clause(SqlClause::SetOperationOrder),
        ),
        (
            "(SELECT point FROM tenant_telemetry) ORDER BY point",
            clause(SqlClause::SetOperationOrder),
        ),
        (
            "SELECT point FROM tenant_telemetry WHERE point IN ((SELECT point FROM tenant_telemetry_1m) LIMIT 1)",
            clause(SqlClause::SetOperationOrder),
        ),
        (
            "SELECT point FROM tenant_telemetry_1m UNION SELECT point FROM tenant_telemetry_1h",
            clause(SqlClause::SetQuantifier),
        ),
        (
            "SELECT point FROM tenant_telemetry LIMIT 1 + 1",
            clause(SqlClause::LimitValue),
        ),
        (
            "SELECT point FROM tenant_telemetry LIMIT -1",
            clause(SqlClause::LimitValue),
        ),
        (
            "SELECT point FROM tenant_telemetry LIMIT 1.5",
            clause(SqlClause::LimitValue),
        ),
        (
            "SELECT point FROM tenant_telemetry OFFSET 1 ROWS",
            clause(SqlClause::LimitValue),
        ),
        (
            "SELECT * FROM LATERAL (SELECT 1) AS x, tenant_telemetry",
            clause(SqlClause::Lateral),
        ),
    ] {
        assert_eq!(refusal(sql), refused, "{sql}");
    }
}

#[test]
fn functions_outside_the_allowlist_are_refused() {
    for (sql, name) in [
        (
            "SELECT getSetting('SQL_tenant_id') FROM tenant_telemetry",
            "getSetting",
        ),
        ("SELECT currentUser() FROM tenant_telemetry", "currentUser"),
        (
            "SELECT dictGet('d', 'a', 1) FROM tenant_telemetry",
            "dictGet",
        ),
        (
            "SELECT joinGet('j', 'a', 1) FROM tenant_telemetry",
            "joinGet",
        ),
        ("SELECT throwIf(1) FROM tenant_telemetry", "throwIf"),
        ("SELECT sleep(1) FROM tenant_telemetry", "sleep"),
        (
            "SELECT hasColumnInTable('trustiot', 'telemetry', 'x') FROM tenant_telemetry",
            "hasColumnInTable",
        ),
        (
            "SELECT arrayJoin([1, 2]) FROM tenant_telemetry",
            "arrayJoin",
        ),
        ("SELECT range(1000000000) FROM tenant_telemetry", "range"),
        ("SELECT today() FROM tenant_telemetry", "today"),
        ("SELECT in(1, 1) FROM tenant_telemetry", "in"),
        (
            "SELECT sumState(value_float) FROM tenant_telemetry",
            "sumState",
        ),
        ("SELECT COLUMNS('x') FROM tenant_telemetry", "COLUMNS"),
        (
            "SELECT toStartOfHOUR(ts, 'UTC') FROM tenant_telemetry",
            "toStartOfHOUR",
        ),
        ("SELECT system.foo(1) FROM tenant_telemetry", "system.foo"),
        (
            "SELECT CURRENT_TIMESTAMP FROM tenant_telemetry",
            "CURRENT_TIMESTAMP",
        ),
    ] {
        assert_eq!(refusal(sql), function(name), "{sql}");
    }
}

/// A word sqlparser reads as the start of an expression becomes a plain name when that reading
/// fails: a misread, refused where it stands.
#[test]
fn a_keyword_the_parser_fell_back_to_a_name_for_is_refused() {
    for (sql, column) in [
        ("SELECT case FROM tenant_telemetry", 8),
        ("SELECT extract, point FROM tenant_telemetry", 8),
        ("SELECT arrayMap(not -> 1, [1]) FROM tenant_telemetry", 17),
    ] {
        assert_eq!(
            refusal(sql),
            SqlRefusal::Syntax {
                line: Some(1),
                column: Some(column)
            },
            "{sql}"
        );
    }
    passes(
        "SELECT point FROM tenant_telemetry WHERE NOT value_bool AND NOT EXISTS (SELECT 1 FROM tenant_telemetry_1m)",
    );
}

/// sqlparser reads a `'…'` string as a name after `AS` and in `WITH`, `WINDOW` and `OVER`, where
/// ClickHouse expects an identifier and refuses the string (the security review of 2026-10-26
/// found these passing the check). sqlparser keeps no position for such a name.
#[test]
fn a_string_where_a_name_belongs_is_refused() {
    let nowhere = SqlRefusal::Syntax {
        line: None,
        column: None,
    };
    for sql in [
        "WITH 'telemetry' AS (SELECT point FROM tenant_telemetry) SELECT point FROM telemetry",
        "WITH 'a' AS (SELECT point FROM tenant_telemetry) SELECT * FROM a",
        "SELECT point AS 'x' FROM tenant_telemetry",
        "SELECT point AS 'x'' FROM system.users --' FROM tenant_telemetry",
        "SELECT * FROM tenant_telemetry AS 'x'",
        "SELECT * FROM (SELECT point FROM tenant_telemetry) AS 'x'",
        "SELECT sum(value_float) OVER w FROM tenant_telemetry WINDOW 'w' AS (ORDER BY ts)",
        "SELECT sum(value_float) OVER 'w' FROM tenant_telemetry WINDOW w AS (ORDER BY ts)",
    ] {
        assert_eq!(refusal(sql), nowhere, "{sql}");
    }
    assert_eq!(
        refusal("SELECT point FROM tenant_telemetry AS t WHERE t.'point' = ''"),
        SqlRefusal::Syntax {
            line: Some(1),
            column: Some(49)
        }
    );
    passes(
        "WITH a AS (SELECT point FROM tenant_telemetry) SELECT sum(1) OVER w AS n FROM a AS b WINDOW w AS (ORDER BY point)",
    );
}

#[test]
fn expressions_outside_the_allowlist_are_refused() {
    for (sql, refused) in [
        // ClickHouse reads a name in an IN list as a table.
        (
            "SELECT 1 FROM tenant_telemetry WHERE 1 IN (system.one)",
            clause(SqlClause::InList),
        ),
        (
            "SELECT 1 FROM tenant_telemetry WHERE point IN (point)",
            clause(SqlClause::InList),
        ),
        (
            "SELECT 1 FROM tenant_telemetry WHERE 1 IN (1 + 1)",
            clause(SqlClause::InList),
        ),
        (
            "SELECT CAST(x AS Date) FROM tenant_telemetry",
            clause(SqlClause::CastType),
        ),
        (
            "SELECT CAST(x AS Nullable(Float64)) FROM tenant_telemetry",
            clause(SqlClause::CastType),
        ),
        (
            "SELECT TRY_CAST(x AS Float64) FROM tenant_telemetry",
            clause(SqlClause::CastType),
        ),
        (
            "SELECT EXTRACT(YEAR FROM ts) FROM tenant_telemetry",
            clause(SqlClause::SpecialSyntax),
        ),
        (
            "SELECT ts AT TIME ZONE 'UTC' FROM tenant_telemetry",
            clause(SqlClause::SpecialSyntax),
        ),
        (
            "SELECT POSITION('a' IN point) FROM tenant_telemetry",
            clause(SqlClause::SpecialSyntax),
        ),
        // `IN` after an operation sqlparser's `POSITION(a IN b)` does not take: a call with one
        // argument `… IN 'a'`. Until 2026-10-26 the first two were an internal error (printed,
        // they read back as `POSITION(a IN b)`) and the `NOT` and `LIKE` ones passed as that
        // call.
        (
            "SELECT position(value_float BETWEEN 1 AND 2 IN 'a') FROM tenant_telemetry",
            clause(SqlClause::SpecialSyntax),
        ),
        (
            "SELECT position((value_int) IS NULL IN 'a') FROM tenant_telemetry",
            clause(SqlClause::SpecialSyntax),
        ),
        (
            "SELECT position(NOT value_bool IN 'a') FROM tenant_telemetry",
            clause(SqlClause::SpecialSyntax),
        ),
        (
            "SELECT position(point LIKE 'a' IN 'b') FROM tenant_telemetry",
            clause(SqlClause::SpecialSyntax),
        ),
        (
            "SELECT position(point) FROM tenant_telemetry",
            clause(SqlClause::SpecialSyntax),
        ),
        (
            "SELECT SUBSTRING(point FROM 1 FOR 2) FROM tenant_telemetry",
            clause(SqlClause::SpecialSyntax),
        ),
        (
            "SELECT trim(point) FROM tenant_telemetry",
            clause(SqlClause::SpecialSyntax),
        ),
        (
            "SELECT DATE '2026-10-01' FROM tenant_telemetry",
            clause(SqlClause::SpecialSyntax),
        ),
        (
            "SELECT CEIL(ts TO DAY) FROM tenant_telemetry",
            clause(SqlClause::SpecialSyntax),
        ),
        (
            "SELECT 1 FROM tenant_telemetry WHERE point LIKE 'a!%' ESCAPE '!'",
            clause(SqlClause::Escape),
        ),
        (
            "SELECT now() - INTERVAL 1.5 DAY FROM tenant_telemetry",
            clause(SqlClause::Interval),
        ),
        (
            "SELECT now() - INTERVAL 1 MICROSECOND FROM tenant_telemetry",
            clause(SqlClause::Interval),
        ),
        (
            "SELECT now() - INTERVAL value_int DAY FROM tenant_telemetry",
            clause(SqlClause::Interval),
        ),
        (
            "SELECT * EXCEPT (point) FROM tenant_telemetry",
            clause(SqlClause::Wildcard),
        ),
        (
            "SELECT sum(*) FROM tenant_telemetry",
            clause(SqlClause::Wildcard),
        ),
        (
            "SELECT count(tenant_telemetry.*) FROM tenant_telemetry",
            clause(SqlClause::Wildcard),
        ),
        (
            "SELECT groupArray(value_float ORDER BY ts) FROM tenant_telemetry",
            clause(SqlClause::AggregateClause),
        ),
        (
            "SELECT first_value(value_float) IGNORE NULLS OVER () FROM tenant_telemetry",
            clause(SqlClause::AggregateClause),
        ),
        (
            "SELECT abs(value_float) OVER () FROM tenant_telemetry",
            clause(SqlClause::Over),
        ),
        (
            "SELECT abs(DISTINCT value_float) FROM tenant_telemetry",
            clause(SqlClause::DistinctArgument),
        ),
        (
            "SELECT sum(1)(value_float) FROM tenant_telemetry",
            clause(SqlClause::Parameters),
        ),
        (
            "SELECT quantile(value_int)(value_float) FROM tenant_telemetry",
            clause(SqlClause::Parameters),
        ),
        (
            "SELECT abs(x -> x) FROM tenant_telemetry",
            clause(SqlClause::Lambda),
        ),
        (
            "SELECT arrayMap([1], x -> x) FROM tenant_telemetry",
            clause(SqlClause::Lambda),
        ),
        (
            "SELECT 1 FROM tenant_telemetry WHERE value_bool IS TRUE",
            clause(SqlClause::Operator),
        ),
        (
            "SELECT 1 FROM tenant_telemetry WHERE point IS DISTINCT FROM 'a'",
            clause(SqlClause::Operator),
        ),
        (
            "SELECT 1 FROM tenant_telemetry WHERE point SIMILAR TO 'a'",
            clause(SqlClause::Operator),
        ),
        (
            "SELECT 1 FROM tenant_telemetry WHERE point REGEXP 'a'",
            clause(SqlClause::Operator),
        ),
        (
            "SELECT 1 FROM tenant_telemetry WHERE value_int XOR 1",
            clause(SqlClause::Operator),
        ),
        (
            "SELECT 1 FROM tenant_telemetry WHERE point LIKE ANY ('a', 'b')",
            clause(SqlClause::Operator),
        ),
        (
            "SELECT ARRAY[1, 2] FROM tenant_telemetry",
            clause(SqlClause::Expression),
        ),
        // A slice needs `:`, which the lexer refuses.
        (
            "SELECT [1, 2][1:2] FROM tenant_telemetry",
            SqlRefusal::LiteralNotAllowed {
                line: 1,
                column: 16,
            },
        ),
    ] {
        assert_eq!(refusal(sql), refused, "{sql}");
    }
}

#[test]
fn time_functions_name_their_time_zone() {
    for (sql, name) in [
        (
            "SELECT toStartOfDay(ts) FROM tenant_telemetry",
            "toStartOfDay",
        ),
        (
            "SELECT toStartOfHour(ts) FROM tenant_telemetry",
            "toStartOfHour",
        ),
        (
            "SELECT toStartOfDay(ts, point) FROM tenant_telemetry",
            "toStartOfDay",
        ),
        (
            "SELECT toStartOfWeek(ts, 'UTC') FROM tenant_telemetry",
            "toStartOfWeek",
        ),
        (
            "SELECT toStartOfInterval(ts, INTERVAL 1 DAY) FROM tenant_telemetry",
            "toStartOfInterval",
        ),
        ("SELECT toDate(ts) FROM tenant_telemetry", "toDate"),
        ("SELECT toYear(ts) FROM tenant_telemetry", "toYear"),
        ("SELECT toYYYYMM(ts) FROM tenant_telemetry", "toYYYYMM"),
        (
            "SELECT formatDateTime(ts, '%F') FROM tenant_telemetry",
            "formatDateTime",
        ),
        (
            "SELECT DATEDIFF('day', ts, now()) FROM tenant_telemetry",
            "dateDiff",
        ),
        (
            "SELECT toDateTime('2026-10-01 00:00:00') FROM tenant_telemetry",
            "toDateTime",
        ),
        (
            "SELECT parseDateTimeBestEffort('2026-10-01') FROM tenant_telemetry",
            "parseDateTimeBestEffort",
        ),
    ] {
        assert_eq!(
            refusal(sql),
            SqlRefusal::TimeZoneRequired {
                function: name.to_owned()
            },
            "{sql}"
        );
    }
}

#[test]
fn lexical_tricks_never_reach_the_parser() {
    for (sql, refused) in [
        // sqlparser reads `'a\\'` as one string, ClickHouse the rest as a base table read.
        (
            "SELECT 'a\\\\', ' FROM trustiot.telemetry --' FROM trustiot.tenant_telemetry",
            SqlRefusal::CharacterNotAllowed {
                line: 1,
                column: 10,
            },
        ),
        (
            "SELECT '\\x41' FROM tenant_telemetry",
            SqlRefusal::CharacterNotAllowed { line: 1, column: 9 },
        ),
        (
            "SELECT 0x1F FROM tenant_telemetry",
            SqlRefusal::LiteralNotAllowed { line: 1, column: 8 },
        ),
        (
            "SELECT 0b101 FROM tenant_telemetry",
            SqlRefusal::LiteralNotAllowed { line: 1, column: 8 },
        ),
        (
            "SELECT `point` FROM tenant_telemetry",
            SqlRefusal::LiteralNotAllowed { line: 1, column: 8 },
        ),
        (
            "SELECT \"point\" FROM tenant_telemetry",
            SqlRefusal::LiteralNotAllowed { line: 1, column: 8 },
        ),
        (
            "SELECT {x:String} FROM tenant_telemetry",
            SqlRefusal::LiteralNotAllowed { line: 1, column: 8 },
        ),
        (
            "SELECT\u{2028}1 FROM tenant_telemetry",
            SqlRefusal::CharacterNotAllowed { line: 1, column: 7 },
        ),
    ] {
        assert_eq!(refusal(sql), refused, "{sql:?}");
    }
}

#[test]
fn syntax_errors_give_a_position_and_never_the_text() {
    assert_eq!(
        refusal("SELECT point\nFROM tenant_telemetry WHERE AND 1"),
        SqlRefusal::Syntax {
            line: Some(2),
            column: Some(33)
        }
    );
    // At the end of the text there is no position.
    assert_eq!(
        refusal("SELECT point FROM tenant_telemetry WHERE"),
        SqlRefusal::Syntax {
            line: None,
            column: None
        }
    );
    for sql in [
        "SELECT * FROM view(SELECT * FROM trustiot.telemetry)",
        "SELECT now() - INTERVAL '1 day' FROM tenant_telemetry",
        "SELECT x ? 1 : 2 FROM tenant_telemetry",
        // Forms the ClickHouse dialect of sqlparser does not parse at all.
        "SELECT count(*) FILTER (WHERE value_float > 1) FROM tenant_telemetry",
        "SELECT * FROM tenant_telemetry_1m AS a ARRAY JOIN [1] AS x",
        "SELECT * FROM tenant_telemetry_1m AS a ASOF JOIN tenant_telemetry_1h AS b ON a.point = b.point AND a.bucket >= b.bucket",
        "SELECT 1 FROM tenant_telemetry WHERE 1 GLOBAL IN (SELECT 1)",
        "SELECT 1 FROM tenant_telemetry INTO OUTFILE 'x'",
        "SELECT 1 FROM tenant_telemetry /* unterminated",
    ] {
        assert!(
            matches!(
                refusal(sql),
                SqlRefusal::Syntax { .. } | SqlRefusal::LiteralNotAllowed { .. }
            ),
            "{sql}"
        );
    }
    match refusal("SELECT 'unterminated FROM tenant_telemetry") {
        SqlRefusal::Syntax { line, column } => {
            assert_eq!((line, column), (Some(1), Some(8)));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(position("x at Line: 3, Column: 14"), (Some(3), Some(14)));
    assert_eq!(
        position("found 'at Line: 9, Column: 9' at Line: 1, Column: 2"),
        (Some(1), Some(2))
    );
    assert_eq!(position("x at Line: 3, Column: 14 extra"), (None, None));
    assert_eq!(position("no position"), (None, None));
}

/// Threads that wait at a gate until the test opens it, and count themselves.
#[derive(Default)]
struct Gate {
    state: std::sync::Mutex<GateState>,
    changed: std::sync::Condvar,
}

#[derive(Default)]
struct GateState {
    open: bool,
    started: usize,
    running: usize,
    most_running: usize,
}

impl Gate {
    /// Counts the calling thread as running and blocks it until the gate opens.
    fn pass(&self) {
        let mut state = self.state.lock().expect("the gate's lock");
        state.started += 1;
        state.running += 1;
        state.most_running = state.most_running.max(state.running);
        self.changed.notify_all();
        while !state.open {
            state = self.changed.wait(state).expect("the gate's lock");
        }
        state.running -= 1;
    }

    fn open(&self) {
        self.state.lock().expect("the gate's lock").open = true;
        self.changed.notify_all();
    }

    fn read<T>(&self, read: impl FnOnce(&GateState) -> T) -> T {
        read(&self.state.lock().expect("the gate's lock"))
    }
}

/// Waits until `done` holds, polling: a generous bound, because a loaded machine starts threads
/// slowly.
async fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
    while !done() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn no_more_checks_run_at_once_than_the_bound() {
    // Permits of this test's own, apart from the service's and the other tests'.
    static PERMITS: Semaphore = Semaphore::const_new(2);
    let gate = std::sync::Arc::new(Gate::default());
    let mut callers = Vec::new();
    for _ in 0..5 {
        let gate = std::sync::Arc::clone(&gate);
        callers.push(tokio::spawn(on_bounded_thread(&PERMITS, move || {
            gate.pass();
        })));
    }
    wait_until("two threads at the gate", || {
        gate.read(|state| state.started == 2)
    })
    .await;
    // The other three wait for a permit: none of them has a thread.
    assert_eq!(PERMITS.available_permits(), 0);
    assert_eq!(gate.read(|state| state.started), 2);
    gate.open();
    for caller in callers {
        caller
            .await
            .expect("the caller does not panic")
            .expect("the thread answers");
    }
    assert_eq!(
        gate.read(|state| (state.started, state.most_running)),
        (5, 2)
    );
    assert_eq!(PERMITS.available_permits(), 2);
}

#[tokio::test]
async fn a_caller_that_stops_waiting_keeps_its_thread_counted() {
    static PERMITS: Semaphore = Semaphore::const_new(1);
    let gate = std::sync::Arc::new(Gate::default());
    let thread_gate = std::sync::Arc::clone(&gate);
    let caller = tokio::spawn(on_bounded_thread(&PERMITS, move || thread_gate.pass()));
    wait_until("the thread at the gate", || {
        gate.read(|state| state.started == 1)
    })
    .await;
    caller.abort();
    assert!(
        caller.await.is_err_and(|error| error.is_cancelled()),
        "the caller was cancelled"
    );
    // The thread still runs, so its permit is still taken.
    assert_eq!(PERMITS.available_permits(), 0);
    gate.open();
    wait_until("the permit back after the thread ends", || {
        PERMITS.available_permits() == 1
    })
    .await;
}
