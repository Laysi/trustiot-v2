//! Internal APIs of the telemetry-query service (`services/telemetry-query`): the read-only SQL
//! an AI writes over the tenant's telemetry (Laysi/trustiot-v2#143).
//!
//! The service checks the SQL itself, without sending anything to ClickHouse first, and runs
//! only the text it printed from the checked tree, as the tenant's read-only account, so the
//! account's row policies, queryable period, settings profile and quota apply. A refusal is a
//! closed set of classes ([`SqlRefusal`]) with fixed details: never ClickHouse's text and never
//! an echo of the SQL, so no free text reaches the model that wrote it.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::client::{Endpoint, Method};
use crate::service;

/// The longest SQL text, in bytes; longer is [`SqlRefusal::TooLong`].
pub const MAX_SQL_BYTES: u32 = 16 * 1024;

/// The rows a call may return when it names no [`RunSql::max_rows`]: the assistant's tool.
pub const DEFAULT_MAX_ROWS: u32 = 200;

/// The largest [`RunSql::max_rows`] a caller may ask for: the dashboards of
/// Laysi/trustiot-v2#150.
pub const MAX_ROWS: u32 = 10_000;

/// The largest result ClickHouse may send for one call, in bytes; larger is
/// [`SqlRefusal::ResultTooLarge`].
pub const MAX_RESULT_BYTES: u64 = 8 * 1024 * 1024;

/// Runs one read-only `SELECT` over the tenant's telemetry views as the caller's tenant.
///
/// Only the services an AI acts through may call it: the request's identity token must come
/// from a token exchange (`act`) by one of them, today `assistant`. The tenant and its role come
/// from that token; every tenant role may read.
///
/// | Outcome | Status | Body |
/// | --- | --- | --- |
/// | Answered | 200 | [`SqlResult`] |
/// | The SQL is refused (a body over the service's limit too), or its answer is | 422 | `validation_failed`, `details` = a [`SqlRefusal`] |
/// | No token, or an invalid or expired one | 401 | `unauthenticated` |
/// | No tenant, or a caller that may not run SQL (no `act`, or another service) | 403 | `permission_denied` |
/// | The tenant's account has not been created yet | 404 | `not_found` |
/// | The tenant's queries are used up for now, or it has as many SQL checks running and waiting as it may | 429 | `rate_limited` |
/// | The body is not this request, or `max_rows` is out of range | 400 | `invalid_request` |
/// | The check failed, or the tenant's account refused a checked query | 500 | `internal` |
/// | The account is not as the service creates it, or Postgres or ClickHouse failed | 503 | `unavailable` |
/// | The query ran longer than the account's limit | 504 | `timeout` |
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunSql {
    /// One `SELECT` (or `WITH … SELECT`), at most [`MAX_SQL_BYTES`].
    pub sql: String,
    /// The most rows the answer may have, 1 to [`MAX_ROWS`]; [`DEFAULT_MAX_ROWS`] when absent.
    /// A query that returns more is refused ([`SqlRefusal::TooManyRows`]), never cut short.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_rows: Option<u32>,
}

impl Endpoint for RunSql {
    type Service = service::TelemetryQuery;
    type Body = Self;
    type Response = SqlResult;
    // A POST that reads: the SQL travels in the body, and nothing is stored.
    const METHOD: Method = Method::Post;
    const ROUTE: &'static str = "/internal/telemetry/v1/sql";

    fn body(&self) -> Option<&Self> {
        Some(self)
    }
}

/// The answer to [`RunSql`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SqlResult {
    /// The SQL the service ran, printed from the checked tree: no comments, every view named
    /// with its database, and without the service's own row limit and output format.
    pub sql: String,
    /// The result's columns, in order.
    pub columns: Vec<SqlColumn>,
    /// The rows, each a JSON array in column order, as ClickHouse's JSON formats write values
    /// (with the pinned ClickHouse, 64-bit integers as JSON numbers and times as
    /// `YYYY-MM-DD hh:mm:ss[.fff]` text).
    pub rows: Vec<Vec<Value>>,
    /// Per view, the time from which the answer holds every row: the tenant's queryable period
    /// cuts earlier rows, in the database.
    pub queryable_from: QueryableFrom,
    /// When the service ran the query.
    pub queried_at: DateTime<Utc>,
}

/// One column of a [`SqlResult`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SqlColumn {
    /// The column's name as ClickHouse gives it.
    pub name: String,
    /// The column's ClickHouse type, for example `Float64` or `DateTime('UTC')`.
    #[serde(rename = "type")]
    pub data_type: String,
}

/// The start of the queryable period of each telemetry view for one answer: rows before it are
/// outside the tenant's period and do not appear. Within five minutes after it, rows may or may
/// not appear.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryableFrom {
    /// `trustiot.tenant_telemetry`, the raw readings.
    pub tenant_telemetry: DateTime<Utc>,
    /// `trustiot.tenant_telemetry_1m`, the one-minute cells.
    pub tenant_telemetry_1m: DateTime<Utc>,
    /// `trustiot.tenant_telemetry_1h`, the one-hour cells.
    pub tenant_telemetry_1h: DateTime<Utc>,
}

/// Why the service refused a [`RunSql`]: the `details` of its 422 `validation_failed`. The JSON
/// is an object whose `class` names the variant in `snake_case`, with the variant's fields
/// beside it, for example `{"class": "function_not_allowed", "function": "dictGet"}`.
///
/// Every field is a number, a fixed name from this module, or a name the SQL itself used that
/// passed the service's identifier rule (`[A-Za-z_][A-Za-z0-9_]*`, at most 128 bytes, joined
/// with `.`); never text from ClickHouse or other parts of the SQL. Callers that turn a refusal
/// into text for a model check those names again.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "class", rename_all = "snake_case")]
pub enum SqlRefusal {
    /// The SQL is longer than [`MAX_SQL_BYTES`].
    TooLong {
        /// [`MAX_SQL_BYTES`].
        max_bytes: u32,
    },
    /// The SQL has too many tokens or nests too deeply.
    TooComplex,
    /// The SQL does not parse. The position is where the parser stopped, when known.
    Syntax {
        /// 1-based line.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        line: Option<u32>,
        /// 1-based column, in characters.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        column: Option<u32>,
    },
    /// A character that is never allowed: a backslash, a control character, a non-ASCII space
    /// or an invisible formatting character.
    CharacterNotAllowed {
        /// 1-based line.
        line: u32,
        /// 1-based column, in characters.
        column: u32,
    },
    /// A token form that is not allowed: a quoted identifier, a string other than `'…'`, a
    /// number other than a plain decimal one, a placeholder, or an operator character outside
    /// the allowed set.
    LiteralNotAllowed {
        /// 1-based line.
        line: u32,
        /// 1-based column, in characters.
        column: u32,
    },
    /// The statement is not a `SELECT`.
    NotASelect,
    /// More than one statement.
    MultipleStatements,
    /// A table other than the telemetry views, or a common table expression named like one.
    TableNotAllowed {
        /// The name as written.
        table: String,
    },
    /// A function outside the allowlist.
    FunctionNotAllowed {
        /// The name as written.
        function: String,
    },
    /// A clause or construct that is not allowed.
    ClauseNotAllowed {
        /// Which one.
        clause: SqlClause,
    },
    /// The query reads none of the telemetry views.
    NoTenantView,
    /// A function that buckets or formats time without an explicit time zone argument.
    TimeZoneRequired {
        /// The function's name.
        function: String,
    },
    /// The answer has more rows than the call allowed.
    TooManyRows {
        /// The call's `max_rows`.
        max: u32,
    },
    /// The answer is larger than [`MAX_RESULT_BYTES`].
    ResultTooLarge {
        /// [`MAX_RESULT_BYTES`].
        max_bytes: u64,
    },
    /// The query read too many rows, or needed too much memory, for the tenant's limits.
    QueryTooHeavy,
    /// ClickHouse could not run the checked query.
    QueryError {
        /// What kind of mistake, from ClickHouse's error code.
        kind: QueryErrorKind,
    },
    /// A class this version does not know: a newer service sent it.
    #[serde(other)]
    Unrecognized,
}

/// The clauses and constructs a [`SqlRefusal::ClauseNotAllowed`] names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SqlClause {
    /// `SETTINGS`.
    Settings,
    /// `FORMAT`.
    Format,
    /// `INTO OUTFILE` or `SELECT … INTO`.
    Into,
    /// `FINAL`.
    Final,
    /// `SAMPLE`.
    Sample,
    /// `PREWHERE`.
    Prewhere,
    /// `WITH TOTALS`.
    WithTotals,
    /// `WITH FILL`.
    WithFill,
    /// `INTERPOLATE`.
    Interpolate,
    /// `DISTINCT ON`.
    DistinctOn,
    /// `QUALIFY`.
    Qualify,
    /// `FROM … SELECT`.
    FromFirst,
    /// `WITH RECURSIVE`.
    RecursiveCte,
    /// A common table expression with a column list, or `MATERIALIZED`.
    CteForm,
    /// `LATERAL`.
    Lateral,
    /// `TOP`.
    Top,
    /// `FETCH`.
    Fetch,
    /// `FOR UPDATE` and other locks.
    Locks,
    /// A join kind other than inner, left, right, full, cross, comma, semi and anti:
    /// `ARRAY JOIN`, `ASOF`, `GLOBAL`, `NATURAL`, `PASTE` and the like.
    JoinKind,
    /// A join other than `CROSS` or a comma without `ON` or `USING`.
    JoinWithoutCondition,
    /// `ORDER BY` or `LIMIT` after `UNION`, `INTERSECT` or `EXCEPT`, which ClickHouse applies to
    /// the last `SELECT` only, or after a query in parentheses, which ClickHouse refuses: select
    /// from the query as a subquery, `SELECT * FROM (…) ORDER BY …`.
    SetOperationOrder,
    /// `UNION`, `INTERSECT` or `EXCEPT` without `ALL` or `DISTINCT`, or with `BY NAME`.
    SetQuantifier,
    /// A `LIMIT` or `OFFSET` that is not a non-negative integer literal.
    LimitValue,
    /// `CAST` or `::` to a type other than `Float64`, `Float32`, `Int64`, `Int32`, `UInt64`,
    /// `UInt32` and `String`.
    CastType,
    /// SQL-standard special syntax with a ClickHouse function instead: `EXTRACT`,
    /// `AT TIME ZONE`, `POSITION … IN`, `SUBSTRING … FROM`, `TRIM`, typed literals such as
    /// `DATE '…'`, `CEIL … TO`.
    SpecialSyntax,
    /// `LIKE … ESCAPE`.
    Escape,
    /// An `INTERVAL` that is not an integer literal with a unit from second to year.
    Interval,
    /// An `IN` list with something other than literals.
    InList,
    /// A lambda outside the argument list of a higher-order function.
    Lambda,
    /// A wildcard with `EXCEPT`, `REPLACE` and the like, or `t.*` as a function argument.
    Wildcard,
    /// A named function argument.
    NamedArgument,
    /// `FILTER`, `WITHIN GROUP`, `IGNORE NULLS`, or `ORDER BY` inside a function's arguments.
    AggregateClause,
    /// `OVER` on a function that is not an aggregate or window function.
    Over,
    /// `DISTINCT` arguments to a function that is not an aggregate.
    DistinctArgument,
    /// Parameters (`f(…)(…)`) on a function that takes none, or parameters that are not
    /// literals.
    Parameters,
    /// A table alias without `AS` that is a keyword, such as `FINAL`, `ANY` or `PASTE`.
    KeywordAlias,
    /// An operator outside `+ - * / % = != <> < <= > >= AND OR NOT || IS [NOT] NULL BETWEEN
    /// LIKE ILIKE IN EXISTS`.
    Operator,
    /// A window frame in `GROUPS`.
    WindowFrame,
    /// Any other construct the checker does not allow.
    Expression,
    /// A construct this version does not know: a newer service sent it.
    #[serde(other)]
    Unrecognized,
}

/// What kind of mistake ClickHouse found in a checked query ([`SqlRefusal::QueryError`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryErrorKind {
    /// A column or alias that does not exist.
    UnknownColumn,
    /// A column name that more than one table or alias provides.
    AmbiguousColumn,
    /// A table name that does not resolve, for example a common table expression out of
    /// scope.
    UnknownTable,
    /// A function or operator applied to values of the wrong type, or values without a
    /// common type.
    TypeMismatch,
    /// Wrong arguments: their number, a value out of range, or an unknown time zone.
    BadArguments,
    /// A string that does not parse as the number, date or time it is used as.
    CannotParseValue,
    /// An aggregate used where it is not allowed, or a column neither aggregated nor grouped.
    AggregateMisuse,
    /// An integer division by zero.
    DivisionByZero,
    /// ClickHouse did not parse the checked text.
    Syntax,
    /// Another mistake in the query.
    Other,
    /// A kind this version does not know: a newer service sent it.
    #[serde(other)]
    Unrecognized,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::client::check_route;

    #[test]
    fn the_route_follows_the_rules_and_has_no_path_parameters() {
        let run = RunSql {
            sql: "SELECT 1".to_owned(),
            max_rows: None,
        };
        assert_eq!(check_route(RunSql::ROUTE), Ok(0));
        assert!(run.path_params().is_empty());
        assert_eq!(run.body(), Some(&run));
    }

    #[test]
    fn the_request_leaves_out_an_absent_row_limit_and_ignores_unknown_members() {
        let run = RunSql {
            sql: "SELECT 1".to_owned(),
            max_rows: None,
        };
        assert_eq!(
            serde_json::to_value(&run).expect("serialize"),
            json!({"sql": "SELECT 1"})
        );
        let newer = json!({"sql": "SELECT 1", "max_rows": 10, "purpose": "dashboard"});
        assert_eq!(
            serde_json::from_value::<RunSql>(newer).expect("deserialize"),
            RunSql {
                sql: "SELECT 1".to_owned(),
                max_rows: Some(10),
            }
        );
        assert!(serde_json::from_value::<RunSql>(json!({"max_rows": 1})).is_err());
    }

    #[test]
    fn the_result_uses_the_wire_names() {
        let at = |text: &str| text.parse::<DateTime<Utc>>().expect("time");
        let result = SqlResult {
            sql: "SELECT 1".to_owned(),
            columns: vec![SqlColumn {
                name: "1".to_owned(),
                data_type: "UInt8".to_owned(),
            }],
            rows: vec![vec![json!(1)]],
            queryable_from: QueryableFrom {
                tenant_telemetry: at("2026-10-19T12:05:00Z"),
                tenant_telemetry_1m: at("2026-07-26T12:06:00Z"),
                tenant_telemetry_1h: at("2026-07-26T13:00:00Z"),
            },
            queried_at: at("2026-10-26T12:00:00Z"),
        };
        let value = serde_json::to_value(&result).expect("serialize");
        assert_eq!(
            value,
            json!({
                "sql": "SELECT 1",
                "columns": [{"name": "1", "type": "UInt8"}],
                "rows": [[1]],
                "queryable_from": {
                    "tenant_telemetry": "2026-10-19T12:05:00Z",
                    "tenant_telemetry_1m": "2026-07-26T12:06:00Z",
                    "tenant_telemetry_1h": "2026-07-26T13:00:00Z",
                },
                "queried_at": "2026-10-26T12:00:00Z",
            })
        );
        assert_eq!(
            serde_json::from_value::<SqlResult>(value).expect("deserialize"),
            result
        );
    }

    #[test]
    fn refusals_are_tagged_by_class() {
        for (refusal, wire) in [
            (
                SqlRefusal::TooLong { max_bytes: 16_384 },
                json!({"class": "too_long", "max_bytes": 16_384}),
            ),
            (SqlRefusal::TooComplex, json!({"class": "too_complex"})),
            (
                SqlRefusal::Syntax {
                    line: Some(2),
                    column: Some(7),
                },
                json!({"class": "syntax", "line": 2, "column": 7}),
            ),
            (
                SqlRefusal::Syntax {
                    line: None,
                    column: None,
                },
                json!({"class": "syntax"}),
            ),
            (
                SqlRefusal::CharacterNotAllowed { line: 1, column: 9 },
                json!({"class": "character_not_allowed", "line": 1, "column": 9}),
            ),
            (
                SqlRefusal::LiteralNotAllowed { line: 1, column: 9 },
                json!({"class": "literal_not_allowed", "line": 1, "column": 9}),
            ),
            (SqlRefusal::NotASelect, json!({"class": "not_a_select"})),
            (
                SqlRefusal::MultipleStatements,
                json!({"class": "multiple_statements"}),
            ),
            (
                SqlRefusal::TableNotAllowed {
                    table: "system.users".to_owned(),
                },
                json!({"class": "table_not_allowed", "table": "system.users"}),
            ),
            (
                SqlRefusal::FunctionNotAllowed {
                    function: "dictGet".to_owned(),
                },
                json!({"class": "function_not_allowed", "function": "dictGet"}),
            ),
            (
                SqlRefusal::ClauseNotAllowed {
                    clause: SqlClause::WithTotals,
                },
                json!({"class": "clause_not_allowed", "clause": "with_totals"}),
            ),
            (SqlRefusal::NoTenantView, json!({"class": "no_tenant_view"})),
            (
                SqlRefusal::TimeZoneRequired {
                    function: "toStartOfDay".to_owned(),
                },
                json!({"class": "time_zone_required", "function": "toStartOfDay"}),
            ),
            (
                SqlRefusal::TooManyRows { max: 200 },
                json!({"class": "too_many_rows", "max": 200}),
            ),
            (
                SqlRefusal::ResultTooLarge {
                    max_bytes: 8_388_608,
                },
                json!({"class": "result_too_large", "max_bytes": 8_388_608}),
            ),
            (
                SqlRefusal::QueryTooHeavy,
                json!({"class": "query_too_heavy"}),
            ),
            (
                SqlRefusal::QueryError {
                    kind: QueryErrorKind::UnknownColumn,
                },
                json!({"class": "query_error", "kind": "unknown_column"}),
            ),
        ] {
            assert_eq!(
                serde_json::to_value(&refusal).expect("serialize"),
                wire,
                "{refusal:?}"
            );
            assert_eq!(
                serde_json::from_value::<SqlRefusal>(wire).expect("deserialize"),
                refusal
            );
        }
    }

    #[test]
    fn newer_classes_clauses_and_kinds_decode_as_unrecognized() {
        assert_eq!(
            serde_json::from_value::<SqlRefusal>(json!({"class": "too_slow", "max_ms": 1}))
                .expect("deserialize"),
            SqlRefusal::Unrecognized
        );
        assert_eq!(
            serde_json::from_value::<SqlRefusal>(
                json!({"class": "clause_not_allowed", "clause": "pivot"})
            )
            .expect("deserialize"),
            SqlRefusal::ClauseNotAllowed {
                clause: SqlClause::Unrecognized
            }
        );
        assert_eq!(
            serde_json::from_value::<SqlRefusal>(json!({"class": "query_error", "kind": "x"}))
                .expect("deserialize"),
            SqlRefusal::QueryError {
                kind: QueryErrorKind::Unrecognized
            }
        );
        // A known class with a missing field does not decode: a caller cannot mistake it for
        // another class.
        assert!(
            serde_json::from_value::<SqlRefusal>(json!({"class": "function_not_allowed"})).is_err()
        );
        assert!(serde_json::from_value::<SqlRefusal>(json!({})).is_err());
    }
}
