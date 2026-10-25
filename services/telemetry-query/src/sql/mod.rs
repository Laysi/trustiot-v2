//! The check of an AI's SQL (Laysi/trustiot-v2#143): pure, with no I/O, so a refused statement
//! never reaches ClickHouse.
//!
//! 1. Size: at most `MAX_SQL_BYTES` of the contract ([`lexer`]).
//! 2. Characters: no backslash, control character, non-ASCII space or invisible formatting
//!    character anywhere ([`lexer`]).
//! 3. Tokens: plain identifiers, plain decimal numbers, `'…'` strings, `--` and `/* */`
//!    comments and a fixed set of operators; the first word is `SELECT` or `WITH` (or `(`),
//!    and nothing follows a `;` ([`lexer`]).
//! 4. Parse with sqlparser's ClickHouse dialect and a recursion limit: one query statement.
//! 5. to 7. The walk ([`check`]): an allowlist of clauses, relations (the three tenant views and
//!    common table expressions) and expressions, the function allowlist ([`functions`]), the
//!    time zone rule, and at least one view read. The walk puts every operand that is itself an
//!    operation in parentheses (but a left operand of the same rank, such as `a - b` in
//!    `a - b + c`).
//! 8. Print the checked tree (no comments), parse and check the printed text again and require
//!    the same tree and the same text, then apply the service's row limit and output format.
//!
//! The service sends ClickHouse only the printed text, never what was written. sqlparser and
//! ClickHouse do not read every text alike, and the steps close the two known gaps: the lexer
//! rules leave only token forms both read the same way (step 3), and the parentheses of step 7
//! leave the printed text a single grouping where the two rank operators differently (`||`
//! against `+`, `LIKE` and `BETWEEN` against `=`). `tests/sql_api.rs` checks both against the
//! pinned ClickHouse with `EXPLAIN AST`: a corpus of queries parses alike as written and as
//! printed, and the queries the two rank differently parse as sqlparser grouped them. That is
//! evidence for those queries, not a proof that the two parsers agree on every text.

pub mod check;
pub mod functions;
pub mod lexer;

use std::thread;

use contract::api::telemetry_query::SqlRefusal;
use sqlparser::ast::{
    Expr, FormatClause, Ident, LimitClause, Offset, OffsetRows, Query, SetExpr, Statement,
    TableFactor, TableWithJoins, Value, ValueWithSpan,
};
use sqlparser::dialect::ClickHouseDialect;
use sqlparser::parser::{Parser, ParserError};
use tokio::sync::Semaphore;

use self::check::{Checker, READ_BACK_RECURSION_LIMIT};

/// How deep the parser may recurse.
pub const RECURSION_LIMIT: usize = 50;

/// The stack of the thread a check runs on: sqlparser's recursion, the walk and the printing of
/// a deeply nested tree need more than an async worker's.
pub const STACK_BYTES: usize = 16 * 1024 * 1024;

/// The output format the service appends: a JSON array per row, after one with the column
/// names and one with their types.
pub const OUTPUT_FORMAT: &str = "JSONCompactEachRowWithNamesAndTypes";

/// A checked query.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Checked {
    /// The query as printed from the checked tree, without the service's row limit and output
    /// format: what the caller sees and stores.
    pub canonical: String,
    /// The statement to send: [`Self::canonical`] with the row limit and the output format.
    pub statement: String,
}

/// Why a query is not run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CheckError {
    /// The query breaks a rule: the caller's mistake.
    Refused(SqlRefusal),
    /// The check itself failed, for example the printed text did not read back as the checked
    /// tree. Never the caller's mistake; logged, and nothing is sent.
    Internal(&'static str),
}

impl From<SqlRefusal> for CheckError {
    fn from(refusal: SqlRefusal) -> Self {
        Self::Refused(refusal)
    }
}

/// Checks `sql` and returns the statement that returns at most `max_rows + 1` rows (one more
/// than allowed, so the caller can tell an answer that is too long). Call it on a thread with a
/// [`STACK_BYTES`] stack: [`check_on_thread`].
///
/// # Errors
///
/// [`CheckError::Refused`] for SQL that breaks a rule, [`CheckError::Internal`] when the
/// printed text does not read back as the tree that was checked.
pub fn check(sql: &str, max_rows: u32) -> Result<Checked, CheckError> {
    let tree = checked_tree(sql, RECURSION_LIMIT)?;
    let canonical = tree.to_string();
    read_back(&canonical, &tree)?;
    let limited = limit(tree, u64::from(max_rows) + 1)?;
    let limited_text = limited.to_string();
    read_back(&limited_text, &limited)?;
    let mut formatted = limited;
    formatted.format_clause = Some(FormatClause::Identifier(Ident::new(OUTPUT_FORMAT)));
    let statement = formatted.to_string();
    if statement != format!("{limited_text} FORMAT {OUTPUT_FORMAT}") {
        return Err(CheckError::Internal(
            "the output format did not print after the query",
        ));
    }
    Ok(Checked {
        canonical,
        statement,
    })
}

/// How many checks run at once in a process. Each runs on a thread of its own with a
/// [`STACK_BYTES`] stack; further requests wait for one to finish.
pub const MAX_CONCURRENT_CHECKS: usize = 4;

/// The threads [`check_on_thread`] may run at once.
static CHECKS: Semaphore = Semaphore::const_new(MAX_CONCURRENT_CHECKS);

/// [`check`] on a thread of its own with a [`STACK_BYTES`] stack, at most
/// [`MAX_CONCURRENT_CHECKS`] at a time. The tree is built, checked and dropped there, never on
/// the caller's thread.
///
/// # Errors
///
/// As [`check`], and [`CheckError::Internal`] when the thread cannot start or stops without an
/// answer.
pub async fn check_on_thread(sql: String, max_rows: u32) -> Result<Checked, CheckError> {
    on_bounded_thread(&CHECKS, move || check(&sql, max_rows)).await?
}

/// Runs `work` on a new thread with a [`STACK_BYTES`] stack once `permits` has one free. The
/// thread holds the permit until `work` returns, also when the caller stops waiting, so no more
/// threads run than `permits` allows.
async fn on_bounded_thread<T: Send + 'static>(
    permits: &'static Semaphore,
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, CheckError> {
    let permit = permits
        .acquire()
        .await
        .map_err(|_| CheckError::Internal("the check's permits are closed"))?;
    let (answer, answered) = tokio::sync::oneshot::channel();
    thread::Builder::new()
        .name("sql-check".to_owned())
        .stack_size(STACK_BYTES)
        .spawn(move || {
            let result = work();
            drop(permit);
            // The receiver is gone only when the caller stopped waiting.
            let _ = answer.send(result);
        })
        .map_err(|_| CheckError::Internal("the check's thread did not start"))?;
    answered
        .await
        .map_err(|_| CheckError::Internal("the check's thread stopped without an answer"))
}

/// Steps 1 to 7: the tokens, the parse with `recursion_limit` and the walk.
fn checked_tree(sql: &str, recursion_limit: usize) -> Result<Query, SqlRefusal> {
    let tokens = lexer::tokens(sql)?;
    let mut statements = Parser::new(&ClickHouseDialect {})
        .with_recursion_limit(recursion_limit)
        .with_tokens_with_locations(tokens)
        .parse_statements()
        .map_err(|error| parse_error(&error))?;
    let statement = match (statements.pop(), statements.is_empty()) {
        (Some(statement), true) => statement,
        (Some(_), false) => return Err(SqlRefusal::MultipleStatements),
        (None, _) => return Err(SqlRefusal::NotASelect),
    };
    let Statement::Query(mut query) = statement else {
        return Err(SqlRefusal::NotASelect);
    };
    Checker::statement(&mut query)?;
    Ok(*query)
}

/// Step 8's read-back: `text` must pass the whole check again and give `tree`, and print as
/// `text` again.
///
/// The printed text is longer, has more tokens and nests deeper than what was written
/// (`trustiot.` before each view, `AS`, the parentheses of step 7). The deeper nesting is
/// bounded by [`check::MAX_ADDED_NESTING`] and allowed for by the read-back's own recursion
/// limit, but the size and the token count are not: a query close to them can pass them as
/// written and not as printed. That read-back refusal (too long, too complex) is the caller's
/// query being too long or too complex, and is returned as such. Any other is a failure of the
/// check.
fn read_back(text: &str, tree: &Query) -> Result<(), CheckError> {
    let again = checked_tree(text, READ_BACK_RECURSION_LIMIT).map_err(|refusal| match refusal {
        SqlRefusal::TooLong { .. } | SqlRefusal::TooComplex => CheckError::Refused(refusal),
        _ => CheckError::Internal("the printed query does not pass the check again"),
    })?;
    if &again != tree || again.to_string() != text {
        return Err(CheckError::Internal(
            "the printed query reads back as a different tree",
        ));
    }
    Ok(())
}

/// The refusal of a parse error: its position only, never its text, which quotes the SQL.
fn parse_error(error: &ParserError) -> SqlRefusal {
    match error {
        ParserError::RecursionLimitExceeded => SqlRefusal::TooComplex,
        ParserError::ParserError(message) | ParserError::TokenizerError(message) => {
            let (line, column) = position(message);
            SqlRefusal::Syntax { line, column }
        }
    }
}

/// The ` at Line: <n>, Column: <m>` that sqlparser ends a message with.
fn position(message: &str) -> (Option<u32>, Option<u32>) {
    const AT: &str = " at Line: ";
    let parsed = message
        .rfind(AT)
        .and_then(|at| message.get(at + AT.len()..))
        .and_then(|rest| rest.split_once(", Column: "))
        .and_then(|(line, column)| Some((line.parse().ok()?, column.parse().ok()?)));
    match parsed {
        Some((line, column)) => (Some(line), Some(column)),
        None => (None, None),
    }
}

/// `query` returning at most `rows` rows. A plain `SELECT` takes the smaller of its own `LIMIT`
/// and `rows` (its `OFFSET` stays); a set operation, a `LIMIT … BY` or a parenthesized query is
/// wrapped in `SELECT * FROM (…) LIMIT rows`.
fn limit(mut query: Query, rows: u64) -> Result<Query, CheckError> {
    let in_place = matches!(*query.body, SetExpr::Select(_))
        && match &query.limit_clause {
            Some(LimitClause::LimitOffset { limit_by, .. }) => limit_by.is_empty(),
            Some(LimitClause::OffsetCommaLimit { .. }) | None => true,
        };
    if in_place {
        query.limit_clause = Some(match query.limit_clause.take() {
            None => LimitClause::LimitOffset {
                limit: Some(number(rows)),
                offset: None,
                limit_by: Vec::new(),
            },
            Some(LimitClause::LimitOffset {
                limit,
                offset,
                limit_by,
            }) => LimitClause::LimitOffset {
                limit: Some(number(smaller(limit.as_ref(), rows))),
                offset,
                limit_by,
            },
            Some(LimitClause::OffsetCommaLimit { offset, limit }) => LimitClause::LimitOffset {
                limit: Some(number(smaller(Some(&limit), rows))),
                offset: Some(Offset {
                    value: offset,
                    rows: OffsetRows::None,
                }),
                limit_by: Vec::new(),
            },
        });
        return Ok(query);
    }
    let template = Parser::parse_sql(&ClickHouseDialect {}, "SELECT * FROM (SELECT 1) LIMIT 1")
        .map_err(|_| CheckError::Internal("the wrapper does not parse"))?;
    let Some(Statement::Query(mut outer)) = template.into_iter().next() else {
        return Err(CheckError::Internal("the wrapper is not a query"));
    };
    let SetExpr::Select(select) = outer.body.as_mut() else {
        return Err(CheckError::Internal("the wrapper is not a SELECT"));
    };
    let Some(TableWithJoins {
        relation: TableFactor::Derived { subquery, .. },
        ..
    }) = select.from.first_mut()
    else {
        return Err(CheckError::Internal("the wrapper reads no subquery"));
    };
    **subquery = query;
    outer.limit_clause = Some(LimitClause::LimitOffset {
        limit: Some(number(rows)),
        offset: None,
        limit_by: Vec::new(),
    });
    Ok(*outer)
}

/// The smaller of a checked `LIMIT` (an integer literal; one too large for a `u64` counts as
/// no limit) and `rows`.
fn smaller(limit: Option<&Expr>, rows: u64) -> u64 {
    match limit {
        Some(Expr::Value(ValueWithSpan {
            value: Value::Number(text, false),
            ..
        })) => text.parse::<u64>().map_or(rows, |limit| limit.min(rows)),
        _ => rows,
    }
}

fn number(value: u64) -> Expr {
    Expr::Value(Value::Number(value.to_string(), false).into())
}

#[cfg(test)]
mod tests;
