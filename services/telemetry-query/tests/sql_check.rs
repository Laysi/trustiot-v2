//! The AI SQL check (Laysi/trustiot-v2#143) against the inputs that may not be written in
//! `services/*/src`, so they live here rather than with the check's unit tests in `src/sql/`:
//!
//! - statements that change a schema, which repo-check rejects anywhere in `services/*/src`;
//! - statements that change the role (`SET ROLE`), and SQL the test builds with `format!` (long,
//!   deep or wide queries), which sql-check rejects in `src` (tools/sql-check, decision 29). This
//!   file is on sql-check's exception list for those two rules: the text is ClickHouse SQL that
//!   the check must refuse or size up, never Postgres SQL, and nothing here talks to Postgres
//!   but a counting port.
//!
//! No container: the check is pure, and a refused statement never reaches ClickHouse. The
//! endpoint's tests run the service's real router with Postgres and ClickHouse as loopback ports
//! that count connections.

use std::any::TypeId;
use std::cell::Cell;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use contract::api::telemetry_query::{MAX_SQL_BYTES, RunSql, SqlRefusal};
use contract::client::Endpoint as _;
use contract::identity::{Actor, INTERNAL_IDENTITY_HEADER};
use contract::{InternalClaims, ServiceName, TenantGrant, TenantId, TenantRole};
use serde_json::{Value, json};
use service_kit::tenant::TenantDb;
use sqlparser::ast::{Expr, Statement};
use sqlparser::dialect::{ClickHouseDialect, Dialect};
use sqlparser::parser::{Parser, ParserError};
use sqlparser::tokenizer::{Token, TokenWithSpan, Tokenizer};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgSslMode};
use telemetry_query::accounts::Accounts;
use telemetry_query::clickhouse::ClickHouse;
use telemetry_query::sql::check::{
    MAX_ADDED_NESTING, READ_BACK_MAX_NESTING, READ_BACK_RECURSION_LIMIT,
};
use telemetry_query::sql::lexer::{
    MAX_IDENTIFIER_BYTES, MAX_NESTING, MAX_TOKENS, tokens, tokens_nested,
};
use telemetry_query::sql::{CheckError, RECURSION_LIMIT, STACK_BYTES, check, check_on_thread};
use testkit::identity::TestIssuer;
use tokio::net::TcpListener;

async fn refusal(sql: &str) -> SqlRefusal {
    match check_on_thread(TenantId::generate(), sql.to_owned(), 200).await {
        Err(CheckError::Refused(refusal)) => refusal,
        other => panic!("{sql}: expected a refusal, got {other:?}"),
    }
}

async fn passes(sql: &str) {
    if let Err(error) = check_on_thread(TenantId::generate(), sql.to_owned(), 200).await {
        panic!("{sql}: {error:?}");
    }
}

/// What the lexer alone refuses `sql` with: steps 1 to 3 of the check.
fn lexer_refusal(sql: &str) -> SqlRefusal {
    tokens(sql).expect_err(sql)
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
        let checked = check_on_thread(TenantId::generate(), sql.to_owned(), 200)
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
    let checked = check_on_thread(TenantId::generate(), sql.to_owned(), 200)
        .await
        .unwrap_or_else(|error| panic!("{sql}: {error:?}"));
    assert_eq!(
        checked.canonical,
        "SELECT 'DROP TABLE trustiot.telemetry' AS text FROM trustiot.tenant_telemetry"
    );
}

// The lexer (steps 1 to 3), on its own.

#[test]
fn the_lexer_limits_the_size_and_the_tokens() {
    let long = format!("SELECT '{}'", "x".repeat(MAX_SQL_BYTES as usize));
    assert_eq!(
        lexer_refusal(&long),
        SqlRefusal::TooLong {
            max_bytes: MAX_SQL_BYTES
        }
    );
    let many = format!("SELECT {}", vec!["1"; MAX_TOKENS].join(","));
    assert_eq!(lexer_refusal(&many), SqlRefusal::TooComplex);
    // Spaces and comments do not count as tokens.
    let spaced = format!("SELECT 1{}", " ".repeat(10_000));
    assert!(tokens(&spaced).is_ok());
}

#[test]
fn the_lexer_refuses_an_identifier_longer_than_allowed() {
    let long = format!("SELECT {}", "x".repeat(MAX_IDENTIFIER_BYTES + 1));
    assert_eq!(
        lexer_refusal(&long),
        SqlRefusal::LiteralNotAllowed { line: 1, column: 8 }
    );
    assert!(tokens(&format!("SELECT {}", "x".repeat(MAX_IDENTIFIER_BYTES))).is_ok());
}

#[test]
fn the_lexer_lets_only_a_select_start_and_only_one() {
    for sql in [
        "SET ROLE NONE",
        "SET DEFAULT ROLE NONE TO tenant",
        "SET max_threads = 1",
        "INSERT INTO t VALUES (1)",
        "ALTER USER x MODIFY SETTINGS y = 1",
        "GRANT SELECT ON *.* TO x",
        "SYSTEM DROP DNS CACHE",
        "KILL QUERY WHERE 1",
        "EXPLAIN SELECT 1",
        "SHOW TABLES",
        "DESCRIBE trustiot.telemetry",
        "VALUES (1)",
        "",
        "  -- only a comment",
        ";",
    ] {
        assert_eq!(lexer_refusal(sql), SqlRefusal::NotASelect, "{sql}");
    }
    for sql in [
        "SELECT 1; SELECT 2",
        "SELECT 1; SET DEFAULT ROLE NONE TO tenant",
        "SELECT 1;;",
        "(SELECT 1); SELECT 2",
    ] {
        assert_eq!(lexer_refusal(sql), SqlRefusal::MultipleStatements, "{sql}");
    }
    assert!(tokens("SELECT 1 ; -- done").is_ok());
}

// The whole check.

#[tokio::test]
async fn statements_other_than_one_select_are_refused_before_parsing() {
    for sql in [
        "SET ROLE NONE",
        "SET DEFAULT ROLE NONE TO tenant",
        "SET max_threads = 1",
        "INSERT INTO trustiot.telemetry SELECT * FROM trustiot.telemetry",
        "ALTER USER x MODIFY SETTINGS SQL_queryable_raw_days = 36500",
        "DROP ROW POLICY p ON trustiot.telemetry",
        "CREATE USER x",
        "GRANT SELECT ON trustiot.telemetry TO x",
        "SYSTEM RELOAD CONFIG",
        "KILL QUERY WHERE 1",
        "OPTIMIZE TABLE trustiot.telemetry FINAL",
        "EXPLAIN SELECT 1",
        "SHOW GRANTS",
        "DESCRIBE TABLE trustiot.telemetry",
        "USE system",
        "EXECUTE AS x SELECT 1",
    ] {
        assert_eq!(refusal(sql).await, SqlRefusal::NotASelect, "{sql}");
    }
    for sql in [
        "SELECT 1 FROM tenant_telemetry; SET DEFAULT ROLE NONE TO x",
        "SELECT 1 FROM tenant_telemetry; SELECT 2 FROM tenant_telemetry",
    ] {
        assert_eq!(refusal(sql).await, SqlRefusal::MultipleStatements, "{sql}");
    }
    // Inside a query the parser still finds only queries.
    assert_eq!(
        refusal("WITH x AS (SELECT 1) INSERT INTO t SELECT * FROM x").await,
        SqlRefusal::NotASelect
    );
}

#[tokio::test]
async fn deep_and_long_queries_are_too_complex_and_never_overflow_the_stack() {
    let nested = format!(
        "SELECT {}1{} FROM tenant_telemetry",
        "(".repeat(200),
        ")".repeat(200)
    );
    // The lexer's nesting limit stops both before the parser.
    let subqueries = format!(
        "SELECT * FROM {}tenant_telemetry{}",
        "(SELECT * FROM ".repeat(100),
        ")".repeat(100)
    );
    for sql in [&nested, &subqueries] {
        assert_eq!(lexer_refusal(sql), SqlRefusal::TooComplex, "{sql}");
        assert_eq!(refusal(sql).await, SqlRefusal::TooComplex, "{sql}");
    }
    // A left-deep chain the parser builds in a loop: the walk's depth limit stops it.
    let chain = format!(
        "SELECT {} FROM tenant_telemetry",
        vec!["x"; 2_000].join("+")
    );
    assert_eq!(refusal(&chain).await, SqlRefusal::TooComplex);
    // A long chain of `NOT` nests without parentheses: the parser's recursion limit stops it;
    // inside a speculative parse the limit shows as a syntax error.
    let not = format!("SELECT {}1 FROM tenant_telemetry", "NOT ".repeat(1_000));
    assert!(
        matches!(
            refusal(&not).await,
            SqlRefusal::TooComplex | SqlRefusal::Syntax { .. }
        ),
        "{not}"
    );
    // Within the limits, a long query passes.
    let wide = format!(
        "SELECT {} FROM tenant_telemetry",
        vec!["value_int + 1"; 500].join(", ")
    );
    passes(&wide).await;
}

/// A query of the given size, built from one repeated shape.
type Shape = fn(usize) -> String;

/// A query that puts the given select list inside the given number of levels.
type Around = fn(usize, &str) -> String;

/// Whether sqlparser alone, with the check's recursion limit, reads `sql` with a column named
/// `NOT` whose alias is `value_bool`. On a thread with the check's stack, as the check parses.
fn misreads_not(sql: String) -> bool {
    std::thread::Builder::new()
        .stack_size(STACK_BYTES)
        .spawn(move || {
            Parser::new(&ClickHouseDialect {})
                .with_recursion_limit(RECURSION_LIMIT)
                .try_with_sql(&sql)
                .and_then(|mut parser| parser.parse_statements())
                .is_ok_and(|statements| {
                    statements
                        .iter()
                        .any(|statement| statement.to_string().contains("NOT AS value_bool"))
                })
        })
        .expect("a thread")
        .join()
        .expect("the parse returns")
}

/// The printed text is longer and nests deeper than what was written: `trustiot.` before each
/// view and the parentheses around operands. A query within the limits as written and over them
/// as printed is the caller's too long or too complex query, never a failure of the check (the
/// security review of 2026-10-26 on Laysi/trustiot-v2#143 found the first three as a 500).
#[tokio::test]
async fn a_query_over_the_limits_only_as_printed_is_refused_as_such() {
    let minus = format!("SELECT {}1 FROM tenant_telemetry", "- ".repeat(24));
    let deeper = format!(
        "SELECT {}1 FROM tenant_telemetry",
        "- ".repeat(MAX_ADDED_NESTING + 2)
    );
    let mut long = String::from("SELECT tenant_telemetry.x");
    while long.len() + ", tenant_telemetry.x FROM tenant_telemetry".len() <= MAX_SQL_BYTES as usize
    {
        long.push_str(", tenant_telemetry.x");
    }
    long.push_str(" FROM tenant_telemetry");
    let joins = format!(
        "SELECT point FROM tenant_telemetry AS t0 {}",
        (1..=350)
            .map(|n| format!("JOIN tenant_telemetry AS t{n} USING (point) "))
            .collect::<String>()
    );
    let comparisons = format!(
        "SELECT point FROM tenant_telemetry WHERE {}",
        vec!["point = 'a'"; 60].join(" = ")
    );
    for (name, sql, passes) in [
        ("24 unary minus signs", minus, true),
        ("a column list up to the size limit", long, false),
        ("350 joins", joins, false),
        (
            "more unary minus signs than the rewrite may nest",
            deeper,
            false,
        ),
        ("60 chained comparisons", comparisons, false),
    ] {
        assert!(sql.len() <= MAX_SQL_BYTES as usize, "{name}: {}", sql.len());
        assert!(tokens(&sql).is_ok(), "{name}: within the lexer's limits");
        match check_on_thread(TenantId::generate(), sql, 200).await {
            Ok(_) if passes => {}
            Err(CheckError::Refused(SqlRefusal::TooLong { .. } | SqlRefusal::TooComplex))
                if !passes => {}
            other => panic!("{name}: passes {passes}, got {other:?}"),
        }
    }
    // At the parser's recursion limit, sqlparser reads the last `NOT` of a chain as a column
    // named `NOT` with the operand as its alias, a tree that depends on the limit: refused,
    // never run as that column. Within the nesting limit, subqueries and a chain of `NOT`s
    // still reach it.
    let mut misread = 0;
    for subqueries in [0, MAX_NESTING] {
        for nots in 1..=60 {
            let sql = format!(
                "{}SELECT {}value_bool FROM tenant_telemetry{}",
                "SELECT * FROM (".repeat(subqueries),
                "NOT ".repeat(nots),
                ")".repeat(subqueries)
            );
            if misreads_not(sql.clone()) {
                misread += 1;
            }
            match check_on_thread(TenantId::generate(), sql.clone(), 200).await {
                Ok(checked) => assert!(!checked.canonical.contains("NOT AS"), "{sql}"),
                Err(CheckError::Refused(_)) => {}
                Err(other) => panic!("{sql}: {other:?}"),
            }
        }
    }
    assert!(misread >= 2, "the chains reach sqlparser's misreading");
    // Each shape the printing nests, at every size up to well past the parser's recursion
    // limit: it passes or is refused, never a failure of the check.
    let shapes: [(&str, Shape); 8] = [
        ("unary minus", |n| {
            format!("SELECT {}value_int FROM tenant_telemetry", "-".repeat(n))
        }),
        ("NOT", |n| {
            format!(
                "SELECT {}value_bool FROM tenant_telemetry",
                "NOT ".repeat(n)
            )
        }),
        ("IS NULL", |n| {
            format!(
                "SELECT value_int{} FROM tenant_telemetry",
                " IS NULL".repeat(n)
            )
        }),
        ("comparisons in a call", |n| {
            format!(
                "SELECT if({}, 1, 0) FROM tenant_telemetry",
                vec!["1"; n + 1].join(" = ")
            )
        }),
        ("BETWEEN", |n| {
            format!(
                "SELECT 1{} FROM tenant_telemetry",
                " BETWEEN 0 AND 2".repeat(n)
            )
        }),
        ("mixed ranks", |n| {
            format!(
                "SELECT 1{} FROM tenant_telemetry",
                " * 2 + 3 || 'x'".repeat(n)
            )
        }),
        ("casts with ::", |n| {
            format!(
                "SELECT value_int{} FROM tenant_telemetry",
                "::Float64".repeat(n)
            )
        }),
        ("LIKE", |n| {
            format!("SELECT 'a'{} FROM tenant_telemetry", " LIKE 'a'".repeat(n))
        }),
    ];
    // Inside written parentheses or subqueries, as deep as the lexer allows and one more.
    let around: [(&str, Around); 2] = [
        ("parentheses", |p, inner| {
            format!(
                "SELECT {}{inner}{} FROM tenant_telemetry",
                "(".repeat(p),
                ")".repeat(p)
            )
        }),
        ("subqueries", |p, inner| {
            format!(
                "{}SELECT {inner} FROM tenant_telemetry{}",
                "SELECT * FROM (".repeat(p),
                ")".repeat(p)
            )
        }),
    ];
    for (name, shape) in shapes {
        for n in 1..=120 {
            let sql = shape(n);
            if let Err(CheckError::Internal(reason)) =
                check_on_thread(TenantId::generate(), sql.clone(), 200).await
            {
                panic!("{name} × {n}: {reason}\n{sql}");
            }
        }
        for (outer, wrap) in around {
            for p in 0..=MAX_NESTING + 1 {
                for n in [
                    1,
                    MAX_ADDED_NESTING - 1,
                    MAX_ADDED_NESTING,
                    MAX_ADDED_NESTING + 1,
                ] {
                    let inner = shape(n);
                    let inner = inner
                        .strip_prefix("SELECT ")
                        .and_then(|rest| rest.split_once(" FROM tenant_telemetry"))
                        .map_or_else(|| "1".to_owned(), |(columns, _)| columns.to_owned());
                    let sql = wrap(p, &inner);
                    if let Err(CheckError::Internal(reason)) =
                        check_on_thread(TenantId::generate(), sql.clone(), 200).await
                    {
                        panic!("{name} × {n} in {p} {outer}: {reason}\n{sql}");
                    }
                }
            }
        }
    }
}

#[tokio::test]
async fn the_check_runs_on_its_own_thread() {
    let checked = check_on_thread(
        TenantId::generate(),
        "SELECT point FROM tenant_telemetry".to_owned(),
        5,
    )
    .await
    .expect("passes");
    assert_eq!(
        checked.statement,
        "SELECT point FROM trustiot.tenant_telemetry LIMIT 6 FORMAT JSONCompactEachRowWithNamesAndTypes"
    );
    let deep = format!("SELECT {}1{}", "(".repeat(5_000), ")".repeat(5_000));
    assert!(matches!(
        check_on_thread(TenantId::generate(), deep, 5).await,
        Err(CheckError::Refused(SqlRefusal::TooComplex))
    ));
}

// The nesting rules and sqlparser's speculative parses (finding H1 of the second security
// review of Laysi/trustiot-v2#143, 2026-10-26).

/// The reviewer's query: `levels` calls of `position(…, 'a')`, each inside the one before.
fn nested_positions(levels: usize) -> String {
    format!(
        "SELECT {}value_string{} AS p FROM tenant_telemetry",
        "position(".repeat(levels),
        ", 'a')".repeat(levels)
    )
}

/// How deep the parentheses and brackets of `text` nest (the texts here have none in strings).
fn nesting_of(text: &str) -> usize {
    let (mut depth, mut deepest) = (0_usize, 0_usize);
    for character in text.chars() {
        match character {
            '(' | '[' => {
                depth += 1;
                deepest = deepest.max(depth);
            }
            ')' | ']' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    deepest
}

/// sqlparser 0.63 parsed `position(` as `POSITION(a IN b)` first and again as a call after that
/// failed, so each call inside another doubled the parse: 40 levels would not have finished.
/// The lexer refuses a `position(` inside another before the parser starts, however it is
/// written.
#[tokio::test]
async fn a_position_inside_another_is_refused_by_the_lexer_before_the_parser() {
    let reviewer = nested_positions(40);
    assert!(reviewer.len() < 1_000, "{}", reviewer.len());
    assert_eq!(lexer_refusal(&reviewer), SqlRefusal::TooComplex);
    assert_eq!(refusal(&reviewer).await, SqlRefusal::TooComplex);
    for sql in [
        nested_positions(2),
        "SELECT position(lower(position(value_string, 'a')), 'b') FROM tenant_telemetry".to_owned(),
        "SELECT position(value_string, toString(position(value_string, 'a'))) FROM tenant_telemetry"
            .to_owned(),
        "SELECT POSITION(Position(value_string, 'a'), 'b') FROM tenant_telemetry".to_owned(),
        "SELECT position /* c */ (position\n(value_string, 'a'), 'b') FROM tenant_telemetry"
            .to_owned(),
        "SELECT position([position(value_string, 'a')], 'b') FROM tenant_telemetry".to_owned(),
        "SELECT t.position(t.position(value_string, 'a'), 'b') FROM tenant_telemetry AS t"
            .to_owned(),
        "SELECT position((SELECT position(value_string, 'a') FROM tenant_telemetry), 'b') \
         FROM tenant_telemetry"
            .to_owned(),
        "SELECT position(position('a' IN value_string) IN 'b') FROM tenant_telemetry".to_owned(),
    ] {
        assert_eq!(lexer_refusal(&sql), SqlRefusal::TooComplex, "{sql}");
    }
    // One at a time, beside one another and in any clause, passes.
    for sql in [
        "SELECT position(value_string, 'a') AS p FROM tenant_telemetry",
        "SELECT position(value_string, 'a', 2) AS p FROM tenant_telemetry",
        "SELECT position(value_string, 'a') + position(point, 'b') AS p FROM tenant_telemetry",
        "SELECT point FROM tenant_telemetry WHERE position(value_string, 'a') > 0",
        "SELECT position(value_string, 'a') AS p FROM tenant_telemetry \
         WHERE position(point, 'b') > 0",
    ] {
        passes(sql).await;
    }
    // Only a call counts: the name alone opens nothing.
    assert!(tokens("SELECT point AS position, (position) FROM tenant_telemetry").is_ok());
}

#[tokio::test]
async fn parentheses_and_brackets_nest_at_most_the_limit() {
    let shapes: [(&str, Shape); 5] = [
        ("parentheses", |levels| {
            format!(
                "SELECT {}value_int{} AS v FROM tenant_telemetry",
                "(".repeat(levels),
                ")".repeat(levels)
            )
        }),
        ("calls", |levels| {
            format!(
                "SELECT {}value_int{} AS v FROM tenant_telemetry",
                "abs(".repeat(levels),
                ")".repeat(levels)
            )
        }),
        ("subqueries", |levels| {
            format!(
                "{}SELECT value_int FROM tenant_telemetry{}",
                "SELECT * FROM (".repeat(levels),
                ")".repeat(levels)
            )
        }),
        ("brackets", |levels| {
            format!(
                "SELECT {}1{} AS v FROM tenant_telemetry",
                "[".repeat(levels),
                "]".repeat(levels)
            )
        }),
        ("brackets in parentheses", |levels| {
            let open: String = (0..levels)
                .map(|level| if level % 2 == 0 { '(' } else { '[' })
                .collect();
            let close: String = open
                .chars()
                .rev()
                .map(|open| if open == '(' { ')' } else { ']' })
                .collect();
            format!("SELECT {open}1{close} AS v FROM tenant_telemetry")
        }),
    ];
    for (name, shape) in shapes {
        let deepest = shape(MAX_NESTING);
        assert_eq!(nesting_of(&deepest), MAX_NESTING, "{name}");
        assert!(tokens(&deepest).is_ok(), "{name}: {MAX_NESTING} levels");
        let deeper = shape(MAX_NESTING + 1);
        assert_eq!(lexer_refusal(&deeper), SqlRefusal::TooComplex, "{name}");
        assert_eq!(refusal(&deeper).await, SqlRefusal::TooComplex, "{name}");
    }
    // Queries nest so: at the limit they pass.
    for (_, shape) in &shapes[..3] {
        passes(&shape(MAX_NESTING)).await;
    }
    // The printed text may nest deeper than the written one, by the parentheses the check adds
    // around operands: its read-back allows for them.
    let inner = "value_int + 1 = 2 AND value_float BETWEEN 0 AND 1";
    let sql = format!(
        "SELECT {}{inner}{} AS v FROM tenant_telemetry",
        "(".repeat(MAX_NESTING),
        ")".repeat(MAX_NESTING)
    );
    let checked = check_on_thread(TenantId::generate(), sql, 200)
        .await
        .expect("passes");
    assert!(
        (MAX_NESTING + 1..=READ_BACK_MAX_NESTING).contains(&nesting_of(&checked.canonical)),
        "{}",
        checked.canonical
    );
    // An unmatched parenthesis is the parser's syntax error, not the lexer's.
    for sql in [
        "SELECT 1) FROM tenant_telemetry",
        "SELECT (1 FROM tenant_telemetry",
        "SELECT 1] FROM tenant_telemetry",
    ] {
        assert!(tokens(sql).is_ok(), "{sql}");
        assert!(
            matches!(refusal(sql).await, SqlRefusal::Syntax { .. }),
            "{sql}"
        );
    }
}

/// sqlparser's ClickHouse dialect, counting the expressions the parser starts to read: it asks
/// the dialect's `parse_prefix` first at the start of each one. Every other answer is
/// [`ClickHouseDialect`]'s (the parse is compared with that dialect's below).
#[derive(Debug, Default)]
struct Counting {
    prefixes: Cell<usize>,
}

/// The methods [`ClickHouseDialect`] answers itself, answered as it does.
macro_rules! as_clickhouse {
    ($($method:ident),* $(,)?) => {
        $(fn $method(&self) -> bool {
            ClickHouseDialect {}.$method()
        })*
    };
}

impl Dialect for Counting {
    fn dialect(&self) -> TypeId {
        TypeId::of::<ClickHouseDialect>()
    }

    fn parse_prefix(&self, _parser: &mut Parser) -> Option<Result<Expr, ParserError>> {
        self.prefixes.set(self.prefixes.get() + 1);
        None
    }

    fn is_identifier_start(&self, ch: char) -> bool {
        ClickHouseDialect {}.is_identifier_start(ch)
    }

    fn is_identifier_part(&self, ch: char) -> bool {
        ClickHouseDialect {}.is_identifier_part(ch)
    }

    fn identifier_quote_style(&self, identifier: &str) -> Option<char> {
        ClickHouseDialect {}.identifier_quote_style(identifier)
    }

    as_clickhouse!(
        supports_string_literal_backslash_escape,
        supports_select_wildcard_except,
        describe_requires_table_keyword,
        require_interval_qualifier,
        supports_limit_comma,
        supports_insert_table_function,
        supports_insert_format,
        supports_numeric_literal_underscores,
        supports_partition_by_after_order_by,
        supports_array_join_syntax,
        supports_dictionary_syntax,
        supports_in_unparenthesized_expr,
        supports_lambda_functions,
        supports_from_first_select,
        supports_order_by_all,
        supports_group_by_expr,
        supports_group_by_with_modifier,
        supports_nested_comments,
        supports_optimize_table,
        supports_prewhere,
        supports_with_fill,
        supports_limit_by,
        supports_interpolate,
        supports_settings,
        supports_select_format,
        supports_select_wildcard_replace,
        supports_comma_separated_trim,
    );
}

/// The expressions sqlparser starts to read while parsing `tokens` (as the check does, with
/// `recursion_limit`), and the tokens other than spaces and comments. The parse must be the
/// one [`ClickHouseDialect`] gives.
fn parse_work(tokens: Vec<TokenWithSpan>, recursion_limit: usize, sql: &str) -> (usize, usize) {
    let significant = tokens
        .iter()
        .filter(|token| !matches!(token.token, Token::Whitespace(_) | Token::EOF))
        .count();
    let counting = Counting::default();
    let counted: Result<Vec<Statement>, ParserError> = Parser::new(&counting)
        .with_recursion_limit(recursion_limit)
        .with_tokens_with_locations(tokens.clone())
        .parse_statements();
    let plain = Parser::new(&ClickHouseDialect {})
        .with_recursion_limit(recursion_limit)
        .with_tokens_with_locations(tokens)
        .parse_statements();
    assert_eq!(counted, plain, "{sql}");
    (counting.prefixes.get(), significant)
}

/// Expressions that reach sqlparser's speculative parses or nest in other ways; `@` is where
/// the nested expression goes.
const NESTED_EXPRESSIONS: &[&str] = &[
    "position(@, 'a')",
    "position(@ IN 'a')",
    "position('a' IN @)",
    "t.position(@, 'a')",
    "CAST(@ AS Float64)",
    "CAST(@, 'Float64')",
    "(@)::Float64",
    "toFloat64(@)",
    "extract(@, 'a')",
    "extract(DAY FROM @)",
    "substring(@, 1, 2)",
    "substring(@ FROM 1 FOR 2)",
    "trim(@, 'a')",
    "trim(BOTH 'a' FROM @)",
    "overlay(@, 'a', 1)",
    "overlay(@ PLACING 'a' FROM 1)",
    "ceil(@)",
    "ceil(@ TO DAY)",
    "floor(@, 1)",
    "case(@)",
    "CASE WHEN @ THEN 1 END",
    "CASE @ WHEN 1 THEN 2 END",
    "not(@)",
    "NOT (@)",
    "@ IN (1)",
    "1 IN (@)",
    "1 IN ((@))",
    "EXISTS (SELECT @)",
    "(SELECT @)",
    "[@]",
    "array(@)",
    "(@, 1)",
    "(@)",
    "abs(@)",
    "t.f(@)",
    "t.cast(@ AS Int32)",
    "quantile(0.5)(@)",
    "quantile(@)(1)",
    "Nullable(Float64)(@)",
    "Array(@)",
    "DateTime64(@)",
    "interval(@)",
    "INTERVAL @ DAY",
    "f(INTERVAL @)",
    "f(x VALUE @)",
    "arrayMap(x -> @, [1])",
    "arrayMap((x, y) -> @, [1], [2])",
    "(@)[1]",
    "@ BETWEEN 1 AND 2",
    "1 BETWEEN @ AND 2",
    "(@) IS NULL",
    "(@) AT TIME ZONE 'UTC'",
    "CONVERT(@, Int32)",
    "count(DISTINCT @)",
    "sum(1) OVER (PARTITION BY @)",
    "if(@, 1, 2)",
    "- @",
    "1 + @",
];

/// Queries and table expressions that nest; `@` is where the nested query or table goes.
const NESTED_QUERIES: &[&str] = &[
    "SELECT * FROM (@)",
    "SELECT * FROM (@) AS a",
    "SELECT 1 IN (@)",
    "WITH a AS (@) SELECT * FROM a",
    "(@)",
    "SELECT (@)",
    "SELECT EXISTS (@)",
    "SELECT * FROM t JOIN (@) ON 1",
    "SELECT 1 UNION ALL (@)",
    "SELECT * FROM ((@) AS a JOIN t ON 1)",
];

/// The query `build` gives for the most levels (at most [`MAX_NESTING`]) that the lexer accepts.
fn deepest(build: &dyn Fn(usize) -> String) -> Option<String> {
    (1..=MAX_NESTING)
        .rev()
        .map(build)
        .find(|sql| tokens(sql).is_ok())
}

/// How many expressions sqlparser may start to read per token. The parser remembers where its
/// speculative parses other than `position(`'s failed, so a level reads what is inside it at
/// most once more: at most the nesting plus one readings of each token, with one more for
/// good measure, and twice that under the one `position(` the lexer allows.
const WORK_PER_TOKEN: usize = 2 * (MAX_NESTING + 2);

/// The parse of each query the lexer accepts costs at most [`WORK_PER_TOKEN`] expressions read
/// per token: every construct above nested as deep as the lexer allows, alone and alternating
/// with each other, inside a `position(…, 'a')` or not, around an expression that parses, one
/// that fails and a long array (most tokens at the deepest level, where the re-readings add
/// up). Counted, not timed. The same bound holds for the read-back of what passes: the
/// printed text adds only plain parentheses, which the parser reads once.
#[test]
fn the_parse_reads_each_token_a_bounded_number_of_times() {
    std::thread::Builder::new()
        .stack_size(STACK_BYTES)
        .spawn(parse_work_is_bounded)
        .expect("a thread")
        .join()
        .expect("the bound holds");
}

#[allow(
    clippy::print_stderr,
    reason = "prints the most work per token and the doubling it catches for the test report"
)]
fn parse_work_is_bounded() {
    let long_array = format!("[{}]", vec!["1"; 200].join(", "));
    let mut shapes: Vec<String> = Vec::new();
    for base in ["value_float", "1 +", long_array.as_str()] {
        for outer in ["@", "position(@, 'a')"] {
            for (index, first) in NESTED_EXPRESSIONS.iter().enumerate() {
                let alone = |levels| nest(&[first], levels, base);
                let wrap = |levels| {
                    format!(
                        "SELECT {} AS v FROM tenant_telemetry",
                        outer.replace('@', &alone(levels))
                    )
                };
                shapes.extend(deepest(&wrap));
                // Each ordered pair once per base and outer call.
                for second in &NESTED_EXPRESSIONS[index..] {
                    for pair in [[first, second], [second, first]] {
                        let wrap = |levels| {
                            format!(
                                "SELECT {} AS v FROM tenant_telemetry",
                                outer.replace('@', &nest(&pair, levels, base))
                            )
                        };
                        shapes.extend(deepest(&wrap));
                    }
                }
            }
        }
        for template in NESTED_QUERIES {
            let query = format!("SELECT {base} AS v FROM tenant_telemetry");
            shapes.extend(deepest(&|levels| nest(&[template], levels, &query)));
        }
    }
    let mut worst = (0_usize, 1_usize, String::new());
    let mut read_back = 0_usize;
    for sql in &shapes {
        let written = tokens(sql).expect("the lexer accepts it");
        let (work, significant) = parse_work(written, RECURSION_LIMIT, sql);
        assert!(
            work <= WORK_PER_TOKEN * significant,
            "{work} for {significant} tokens: {sql}"
        );
        if work * worst.1 > worst.0 * significant {
            worst = (work, significant, sql.clone());
        }
        if let Ok(checked) = check(sql, 200) {
            let printed = tokens_nested(&checked.canonical, READ_BACK_MAX_NESTING)
                .expect("the printed text passed its read-back");
            let (work, significant) =
                parse_work(printed, READ_BACK_RECURSION_LIMIT, &checked.canonical);
            assert!(
                work <= WORK_PER_TOKEN * significant,
                "{work} for {significant} tokens: {}",
                checked.canonical
            );
            read_back += 1;
        }
    }
    assert!(shapes.len() > 10_000, "{} shapes", shapes.len());
    assert!(read_back > 1_000, "{read_back} read back");
    eprintln!(
        "{} shapes, {read_back} read back; the most work per token: {} for {} tokens: {}",
        shapes.len(),
        worst.0,
        worst.1,
        worst.2
    );

    // The counting sees the doubling the rule keeps out: nested `position(` calls, parsed
    // without the lexer, read far more than the bound allows at 12 levels.
    let sql = nested_positions(12);
    let unchecked = Tokenizer::new(&ClickHouseDialect {}, &sql)
        .tokenize_with_location()
        .expect("tokens");
    let (work, significant) = parse_work(unchecked, RECURSION_LIMIT, &sql);
    eprintln!("12 nested position calls without the lexer: {work} for {significant} tokens");
    assert!(
        work > 4 * WORK_PER_TOKEN * significant,
        "{work} for {significant} tokens"
    );
}

/// `templates` applied in turn, `levels` times, around `base`.
fn nest(templates: &[&&str], levels: usize, base: &str) -> String {
    let mut text = base.to_owned();
    for level in 0..levels {
        text = templates[level % templates.len()].replace('@', &text);
    }
    text
}

/// The sqlparser version whose speculative parses were surveyed for the nesting rules and the
/// templates above (docs/test-reports/data/2026-10-26-143-ai-sql/parser-survey.txt). The
/// counting test sees only the constructs it lists, so a release that adds a speculative parse
/// none of them reaches would pass it.
const SURVEYED_SQLPARSER: &str = "0.63.0";

/// The guard on sqlparser upgrades. Renovate proposes them in a pull request of their own
/// (renovate.json5), which fails here until someone has redone the survey for the new version
/// (this service's README, section 「升級 sqlparser」) and moved [`SURVEYED_SQLPARSER`]. Every
/// sqlparser in `Cargo.lock` counts, so a second copy that another crate pulls in fails too.
#[test]
fn sqlparser_is_the_version_whose_speculative_parses_were_surveyed() {
    let lock =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Cargo.lock"))
            .expect("read Cargo.lock");
    let versions: Vec<&str> = lock
        .split("[[package]]")
        .filter(|package| package.lines().any(|line| line == "name = \"sqlparser\""))
        .map(|package| {
            package
                .lines()
                .find_map(|line| line.strip_prefix("version = \"")?.strip_suffix('"'))
                .expect("a locked package has a version")
        })
        .collect();
    assert!(!versions.is_empty(), "no sqlparser in Cargo.lock");
    assert!(
        versions
            .iter()
            .all(|version| *version == SURVEYED_SQLPARSER),
        "Cargo.lock resolves sqlparser {versions:?}, but its speculative parses were surveyed for \
         {SURVEYED_SQLPARSER} only. Before this upgrade merges, follow \
         services/telemetry-query/README.md, section 「升級 sqlparser」: (1) list the new \
         version's maybe_parse, try_parse and index-reset sites and compare them with \
         docs/test-reports/data/2026-10-26-143-ai-sql/parser-survey.txt; (2) add the construct \
         of each new or changed site the check can reach to NESTED_EXPRESSIONS or NESTED_QUERIES \
         in this file and run the_parse_reads_each_token_a_bounded_number_of_times; (3) a \
         speculative parse that does not remember its failure and reads again after its inner \
         parse succeeded needs a lexer rule or a work bound first; (4) then set \
         SURVEYED_SQLPARSER to the new version."
    );
}

// The endpoint, through the service's router.

/// A loopback port that accepts connections, counts them and closes them at once: a Postgres or
/// ClickHouse that is never reached unless the count says so.
async fn counting_port() -> (u16, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a loopback port");
    let port = listener.local_addr().expect("its address").port();
    let count = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&count);
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            counted.fetch_add(1, Ordering::SeqCst);
            drop(stream);
        }
    });
    (port, count)
}

/// Serves `router` on a loopback port and returns its origin.
async fn serve(router: Router) -> String {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a loopback port");
    let origin = format!("http://{}", listener.local_addr().expect("its address"));
    tokio::spawn(async move {
        axum::serve(listener, router)
            .await
            .expect("the server runs");
    });
    origin
}

/// The service's router, whose Postgres and ClickHouse are counting ports; its origin and the
/// two counts.
async fn counted_service(issuer: &TestIssuer) -> (String, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let (postgres_port, postgres) = counting_port().await;
    let (clickhouse_port, clickhouse) = counting_port().await;
    let options = PgConnectOptions::new()
        .host("127.0.0.1")
        .port(postgres_port)
        .username("telemetry_query_app")
        .database("trustiot")
        .ssl_mode(PgSslMode::VerifyFull);
    // Short: the positive control below waits for the pool to give up on the closed port.
    let pool = PgPoolOptions::new().acquire_timeout(Duration::from_secs(2));
    let db = TenantDb::connect_lazy(options, pool).expect("verify-full options");
    let client = ClickHouse::new(&format!("http://127.0.0.1:{clickhouse_port}"), None)
        .expect("a ClickHouse client");
    let accounts = Accounts::new(client, "trustiot_telemetry_query", &"p".repeat(32));
    let origin = serve(telemetry_query::history::routes(
        issuer.verifier(ServiceName::TelemetryQuery),
        db,
        accounts,
    ))
    .await;
    (origin, postgres, clickhouse)
}

fn claims(act: Option<ServiceName>, role: Option<TenantRole>) -> InternalClaims {
    let grant = role.map(|role| TenantGrant {
        id: TenantId::generate(),
        role,
    });
    let mut claims = TestIssuer::claims(ServiceName::TelemetryQuery, grant);
    claims.act = act.map(|sub| Actor { sub });
    claims
}

/// Posts `body` to the SQL endpoint with a token for `claims`; the status and the body.
async fn call(
    origin: &str,
    issuer: &TestIssuer,
    claims: &InternalClaims,
    body: &str,
) -> (u16, String) {
    let response = reqwest::Client::new()
        .post(format!("{origin}{}", RunSql::ROUTE))
        .header(INTERNAL_IDENTITY_HEADER, issuer.token(claims))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body.to_owned())
        // Above the 2 s the positive control's pool waits for Postgres.
        .timeout(Duration::from_secs(60))
        .send()
        .await
        .expect("the service answers");
    let status = response.status().as_u16();
    (status, response.text().await.expect("a body"))
}

#[tokio::test]
async fn a_refused_statement_reaches_neither_postgres_nor_clickhouse() {
    let issuer = TestIssuer::new();
    let (origin, postgres, clickhouse) = counted_service(&issuer).await;
    let assistant = claims(Some(ServiceName::Assistant), Some(TenantRole::Viewer));
    for sql in [
        "SET ROLE NONE",
        "SET DEFAULT ROLE NONE TO tenant",
        "SET max_threads = 1",
        "INSERT INTO trustiot.telemetry SELECT * FROM trustiot.telemetry",
        "GRANT SELECT ON trustiot.telemetry TO x",
        "SYSTEM RELOAD CONFIG",
        "KILL QUERY WHERE 1",
        "SELECT 1 FROM tenant_telemetry; SET DEFAULT ROLE NONE TO x",
        "SELECT point FROM tenant_telemetry SETTINGS readonly = 0",
        "SELECT * FROM system.users",
        "SELECT getSetting('SQL_tenant_id') FROM tenant_telemetry",
    ] {
        let body = json!({ "sql": sql }).to_string();
        let (status, answer) = call(&origin, &issuer, &assistant, &body).await;
        assert_eq!(status, 422, "{sql}: {answer}");
    }
    // Callers that may not run SQL, and bodies that are not a request.
    for (claims, body, status) in [
        (
            claims(None, Some(TenantRole::Admin)),
            "{\"sql\":\"SELECT 1\"}",
            403,
        ),
        (
            claims(Some(ServiceName::Assets), Some(TenantRole::Admin)),
            "{\"sql\":\"SELECT 1\"}",
            403,
        ),
        (
            claims(Some(ServiceName::Assistant), None),
            "{\"sql\":\"SELECT 1\"}",
            403,
        ),
        (assistant.clone(), "not json", 400),
        (
            assistant.clone(),
            "{\"sql\":\"SELECT 1\",\"max_rows\":0}",
            400,
        ),
        (
            assistant.clone(),
            "{\"sql\":\"SELECT 1\",\"max_rows\":10001}",
            400,
        ),
        (assistant.clone(), "{\"max_rows\":5}", 400),
    ] {
        let (answered, answer) = call(&origin, &issuer, &claims, body).await;
        assert_eq!(answered, status, "{body}: {answer}");
    }
    assert_eq!(postgres.load(Ordering::SeqCst), 0, "Postgres was reached");
    assert_eq!(
        clickhouse.load(Ordering::SeqCst),
        0,
        "ClickHouse was reached"
    );

    // The control: a statement that passes goes on to Postgres, which the counter sees.
    let body = json!({ "sql": "SELECT point FROM tenant_telemetry" }).to_string();
    let (status, answer) = call(&origin, &issuer, &assistant, &body).await;
    assert_eq!(status, 503, "{answer}");
    assert!(
        postgres.load(Ordering::SeqCst) > 0,
        "the control reached Postgres"
    );
}

#[tokio::test]
async fn sql_over_its_limit_is_too_long_and_never_sent() {
    let issuer = TestIssuer::new();
    let (origin, _, clickhouse) = counted_service(&issuer).await;
    let assistant = claims(Some(ServiceName::Assistant), Some(TenantRole::Viewer));
    let sql = format!("SELECT '{}' FROM tenant_telemetry", "x".repeat(17 * 1024));
    let body = json!({ "sql": sql }).to_string();
    let (status, answer) = call(&origin, &issuer, &assistant, &body).await;
    assert_eq!(status, 422, "{answer}");
    let answer: Value = serde_json::from_str(&answer).expect("a JSON body");
    assert_eq!(
        answer["details"],
        json!({"class": "too_long", "max_bytes": MAX_SQL_BYTES})
    );
    assert_eq!(clickhouse.load(Ordering::SeqCst), 0);
}
