//! Dataset results: one run of one dataset, as the page receives it and as the check is given
//! it.
//!
//! Every source becomes one table ([`DatasetResult`]): a SQL dataset's columns are the query's,
//! and a semantic dataset's response is flattened into the fixed columns of
//! [`semantic_columns`], one flattening per source and interval kind (bucketed or `total`). The
//! assistant builds the table (`services/assistant`), the runtime reads it
//! (`frontend/sandbox/`), and the check compares the page with it.
//!
//! # Cells
//!
//! A cell is JSON `null` or a value of its column's [`ColumnKind`]:
//!
//! - `number`: a JSON number. ClickHouse's `nan` and infinities arrive as `null`.
//! - `text`: a JSON string.
//! - `bool`: `true` or `false`.
//! - `time`: a string in one of three forms, read as a wall-clock time without converting
//!   between zones: `YYYY-MM-DD` (a ClickHouse `Date`), `YYYY-MM-DD hh:mm:ss[.f…]` (a ClickHouse
//!   `DateTime` or `DateTime64`, in the zone the query named) or RFC 3339 with an offset
//!   (`YYYY-MM-DDThh:mm:ss[.f…]±hh:mm` or `Z`, a semantic response's local time). The runtime
//!   keeps the date and time of day as written and ignores the offset, so a time shows as the
//!   query's or the request's zone saw it.
//!
//! # Flattening a semantic response
//!
//! One row per series and bucket (or per series for `total`), series in the request's order
//! and buckets in time order. A reading (`last`, `first`) becomes two columns: the number
//! (a float or an integer, a boolean as 0 or 1) and `…_text` for a text reading; the other is
//! `null`. A bucketed series without readings has no rows; its `total` row has `status`
//! `no_data` and `null` values.

use std::collections::BTreeSet;

use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::ToSchema;
use utoipa::openapi::schema::{Array, ArrayBuilder, ObjectBuilder, SchemaType, Type};

use super::names::{is_column_name, is_name};
use super::spec::{SemanticInterval, SourceKind};
use crate::api::telemetry_query::QueryableFrom;

/// The most rows a dataset may have (decision of 2026-10-26): a dashboard shows a period's
/// figures, not an export. A run that returns more is refused with how to narrow it, never cut.
pub const MAX_DATASET_ROWS: u32 = 2_000;

/// The most columns of a dataset: more than a page can show, few enough to keep a result small.
pub const MAX_COLUMNS: usize = 64;

/// The largest dataset-run response, in bytes (design record 「資料集的執行」): the result as
/// JSON. A larger one is refused ([`crate::api::dashboard::public`]'s
/// `dashboard.dataset_result_too_large`).
pub const MAX_RESULT_BYTES: usize = 2 * 1024 * 1024;

/// The OpenAPI schema of [`DatasetResult::rows`]: arrays of cells, each `null` or a number, a
/// string or a boolean (the module documentation, 「Cells」).
fn rows_schema() -> Array {
    let cell = ObjectBuilder::new()
        .schema_type(
            [Type::Number, Type::String, Type::Boolean, Type::Null]
                .into_iter()
                .collect::<SchemaType>(),
        )
        .description(Some(
            "A cell: `null`, or a value of its column's type: a number, a string (text or a \
             time) or a boolean.",
        ));
    ArrayBuilder::new()
        .description(Some("The rows, each an array of cells in column order."))
        .items(ArrayBuilder::new().items(cell))
        .build()
}

/// What a column holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
#[schema(as = DashboardColumnType)]
pub enum ColumnKind {
    Number,
    Text,
    Time,
    Bool,
}

impl ColumnKind {
    /// The kind of a ClickHouse column type as the SQL endpoint names it (`SqlColumn::data_type`),
    /// or `None` for a type a dashboard cannot use: arrays, tuples, maps, 128- and 256-bit
    /// integers (which ClickHouse writes as strings), `Nothing` and anything newer. The query
    /// converts such a column itself (`toString`, `toFloat64`, `arrayJoin`).
    #[must_use]
    pub fn from_clickhouse_type(data_type: &str) -> Option<Self> {
        let mut base = data_type.trim();
        loop {
            let unwrapped = ["Nullable(", "LowCardinality("]
                .into_iter()
                .find_map(|wrapper| base.strip_prefix(wrapper)?.strip_suffix(')'));
            match unwrapped {
                Some(inner) => base = inner.trim(),
                None => break,
            }
        }
        let name = base.split_once('(').map_or(base, |(name, _)| name);
        match name {
            "Int8" | "Int16" | "Int32" | "Int64" | "UInt8" | "UInt16" | "UInt32" | "UInt64"
            | "Float32" | "Float64" | "BFloat16" | "Decimal" | "Decimal32" | "Decimal64"
            | "Decimal128" | "Decimal256" => Some(Self::Number),
            "String" | "FixedString" | "UUID" | "Enum" | "Enum8" | "Enum16" | "IPv4" | "IPv6" => {
                Some(Self::Text)
            }
            "Date" | "Date32" | "DateTime" | "DateTime64" => Some(Self::Time),
            "Bool" => Some(Self::Bool),
            _ => None,
        }
    }

    /// Whether `value` is a cell of this kind (`null` is a cell of every kind).
    #[must_use]
    pub fn admits(self, value: &Value) -> bool {
        match (self, value) {
            (_, Value::Null)
            | (Self::Number, Value::Number(_))
            | (Self::Bool, Value::Bool(_))
            | (Self::Text, Value::String(_)) => true,
            (Self::Time, Value::String(text)) => is_time_cell(text),
            _ => false,
        }
    }
}

/// One column of a [`DatasetResult`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[schema(as = DashboardResultColumn)]
pub struct ResultColumn {
    /// The column's name ([`is_column_name`]); a formula and a reference name it.
    #[schema(pattern = "^[A-Za-z_][A-Za-z0-9_]{0,63}$")]
    pub name: String,
    #[serde(rename = "type")]
    pub kind: ColumnKind,
}

/// One run of one dataset.
///
/// Internal callers (the check) accept fields they do not know, so fields are only ever added
/// as optional ones (this crate's `README.md`, 「內部 API」).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[schema(as = DashboardDatasetResult)]
pub struct DatasetResult {
    /// The dataset's name in the spec.
    pub dataset: String,
    /// The columns, in order.
    pub columns: Vec<ResultColumn>,
    /// The rows, each an array of cells in column order (see the module documentation).
    #[schema(schema_with = rows_schema)]
    pub rows: Vec<Vec<Value>>,
    /// `rows.len()`.
    pub row_count: u32,
    pub meta: ResultMeta,
}

/// When and how a dataset ran.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[schema(as = DashboardResultMeta)]
pub struct ResultMeta {
    /// When the query ran, with the offset its source gave: UTC for SQL, the request's time zone
    /// for a semantic source.
    pub queried_at: DateTime<FixedOffset>,
    /// How long the run took, in milliseconds, as the assistant measured it.
    pub duration_ms: u32,
    pub source: SourceMeta,
}

/// What the source said about the data's range.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[schema(as = DashboardSourceMeta)]
pub enum SourceMeta {
    /// A SQL dataset: where the tenant's queryable period starts in each view.
    Sql { queryable_from: QueryablePeriod },
    /// A statistics dataset: the range the answer covers.
    TelemetryStatistics { range: SemanticRange },
    /// A usage dataset: the range the answer covers.
    TelemetryUsage { range: SemanticRange },
}

impl SourceMeta {
    /// The source's kind.
    #[must_use]
    pub const fn kind(&self) -> SourceKind {
        match self {
            Self::Sql { .. } => SourceKind::Sql,
            Self::TelemetryStatistics { .. } => SourceKind::TelemetryStatistics,
            Self::TelemetryUsage { .. } => SourceKind::TelemetryUsage,
        }
    }

    /// The range of a semantic source.
    #[must_use]
    pub const fn range(&self) -> Option<&SemanticRange> {
        match self {
            Self::Sql { .. } => None,
            Self::TelemetryStatistics { range } | Self::TelemetryUsage { range } => Some(range),
        }
    }
}

/// The SQL endpoint's `queryable_from`: per telemetry view, the time from which the answer
/// holds every row.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[schema(as = DashboardQueryablePeriod)]
pub struct QueryablePeriod {
    /// `trustiot.tenant_telemetry`, the raw readings.
    pub tenant_telemetry: DateTime<Utc>,
    /// `trustiot.tenant_telemetry_1m`, the one-minute cells.
    pub tenant_telemetry_1m: DateTime<Utc>,
    /// `trustiot.tenant_telemetry_1h`, the one-hour cells.
    pub tenant_telemetry_1h: DateTime<Utc>,
}

impl From<QueryableFrom> for QueryablePeriod {
    fn from(from: QueryableFrom) -> Self {
        Self {
            tenant_telemetry: from.tenant_telemetry,
            tenant_telemetry_1m: from.tenant_telemetry_1m,
            tenant_telemetry_1h: from.tenant_telemetry_1h,
        }
    }
}

/// The aggregate table a semantic answer read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[schema(as = DashboardSemanticResolution)]
pub enum SemanticResolution {
    /// One-minute cells.
    #[serde(rename = "1m")]
    Minute,
    /// One-hour cells.
    #[serde(rename = "1h")]
    Hour,
}

/// The `range` of a statistics or usage answer (Laysi/trustiot-v2#142), field for field, so the
/// assistant reads it straight from the response; `services/telemetry-query/tests` checks that
/// the two stay alike.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[schema(as = DashboardSemanticRange)]
pub struct SemanticRange {
    /// The time zone the buckets follow.
    pub tz: String,
    pub interval: SemanticInterval,
    /// The range read: `from` inclusive, `to` exclusive, in `tz`.
    pub from: DateTime<FixedOffset>,
    pub to: DateTime<FixedOffset>,
    /// Whether `from` was moved to the start of the queryable period.
    pub truncated: bool,
    /// The start of the queryable period of the table read.
    pub earliest: DateTime<FixedOffset>,
    /// Whether the range ends after the request's time, so its last bucket is not complete.
    pub ends_after_now: bool,
    pub resolution: SemanticResolution,
    /// When the request was answered.
    pub queried_at: DateTime<FixedOffset>,
}

/// A column of a semantic dataset's flattened table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FixedColumn {
    pub name: &'static str,
    pub kind: ColumnKind,
}

const fn column(name: &'static str, kind: ColumnKind) -> FixedColumn {
    FixedColumn { name, kind }
}

use ColumnKind::{Bool, Number, Text, Time};

/// `telemetry_statistics` with a bucket interval: one row per series and bucket with readings.
/// `last` is the bucket's latest reading; `min`, `max` and `avg` count a boolean as 0 or 1 and
/// are `null` for a bucket of text readings only.
pub const STATISTICS_BUCKET_COLUMNS: [FixedColumn; 9] = [
    column("device_id", Text),
    column("point", Text),
    column("time", Time),
    column("count", Number),
    column("min", Number),
    column("max", Number),
    column("avg", Number),
    column("last", Number),
    column("last_text", Text),
];

/// `telemetry_statistics` with `interval: total`: one row per series, the series' statistics
/// over the range. `status` is `ok` or `no_data`; `min_time` and `max_time` are the start of the
/// cell the extreme was in.
pub const STATISTICS_TOTAL_COLUMNS: [FixedColumn; 16] = [
    column("device_id", Text),
    column("point", Text),
    column("status", Text),
    column("count", Number),
    column("numeric_count", Number),
    column("min", Number),
    column("min_time", Time),
    column("max", Number),
    column("max_time", Time),
    column("avg", Number),
    column("first", Number),
    column("first_text", Text),
    column("first_time", Time),
    column("last", Number),
    column("last_text", Text),
    column("last_time", Time),
];

/// `telemetry_usage` with a bucket interval: one row per series and bucket with readings (a
/// bucket without readings is unknown, not zero, and has no row).
pub const USAGE_BUCKET_COLUMNS: [FixedColumn; 8] = [
    column("device_id", Text),
    column("point", Text),
    column("time", Time),
    column("usage", Number),
    column("readings", Number),
    column("drops", Number),
    column("after_gap", Bool),
    column("suspicious_steps", Number),
];

/// `telemetry_usage` with `interval: total`: one row per series. `status` is `ok`, `no_data`,
/// `not_numeric` or `not_cumulative`; `total` is `null` unless it is `ok`; `verdict` is the
/// cumulative check's (`cumulative`, `too_few_readings`, `not_cumulative`), `null` without data.
pub const USAGE_TOTAL_COLUMNS: [FixedColumn; 15] = [
    column("device_id", Text),
    column("point", Text),
    column("status", Text),
    column("total", Number),
    column("verdict", Text),
    column("readings", Number),
    column("drops", Number),
    column("gaps", Number),
    column("suspicious_steps", Number),
    column("baseline", Bool),
    column("first_reading", Number),
    column("first_reading_time", Time),
    column("last_reading", Number),
    column("last_reading_time", Time),
    column("skipped_cells", Number),
];

/// The columns of a semantic source's table for `interval`, or `None` for SQL, whose columns
/// are known only after it ran.
#[must_use]
pub fn semantic_columns(
    source: SourceKind,
    interval: SemanticInterval,
) -> Option<&'static [FixedColumn]> {
    let total = interval == SemanticInterval::Total;
    match (source, total) {
        (SourceKind::Sql, _) => None,
        (SourceKind::TelemetryStatistics, false) => Some(&STATISTICS_BUCKET_COLUMNS),
        (SourceKind::TelemetryStatistics, true) => Some(&STATISTICS_TOTAL_COLUMNS),
        (SourceKind::TelemetryUsage, false) => Some(&USAGE_BUCKET_COLUMNS),
        (SourceKind::TelemetryUsage, true) => Some(&USAGE_TOTAL_COLUMNS),
    }
}

/// What is wrong with a [`DatasetResult`]: a closed set, with fixed English text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ResultError {
    #[error("the dataset name is not a dashboard name")]
    DatasetName,
    #[error("a result has no columns, or more than 64")]
    ColumnCount,
    #[error("a column name is not a dashboard column name, or appears twice")]
    ColumnName,
    #[error("a row's length differs from the number of columns")]
    RowWidth,
    #[error("a cell does not match its column's type")]
    CellKind,
    #[error("row_count differs from the number of rows")]
    RowCount,
    #[error("the result has more rows than the dataset allows")]
    TooManyRows,
    #[error("the columns differ from the semantic source's fixed columns")]
    FixedColumns,
}

impl DatasetResult {
    /// Checks the table's shape: the dataset and column names, every row's width, every cell
    /// against its column's kind, `row_count`, at most `max_rows` rows (itself at most
    /// [`MAX_DATASET_ROWS`]), and for a semantic source the fixed columns of its interval.
    ///
    /// # Errors
    ///
    /// The first problem found.
    pub fn validate(&self, max_rows: u32) -> Result<(), ResultError> {
        if !is_name(&self.dataset) {
            return Err(ResultError::DatasetName);
        }
        if self.columns.is_empty() || self.columns.len() > MAX_COLUMNS {
            return Err(ResultError::ColumnCount);
        }
        let mut names = BTreeSet::new();
        if !self
            .columns
            .iter()
            .all(|column| is_column_name(&column.name) && names.insert(column.name.as_str()))
        {
            return Err(ResultError::ColumnName);
        }
        if let Some(range) = self.meta.source.range() {
            let fixed = semantic_columns(self.meta.source.kind(), range.interval).unwrap_or(&[]);
            let same = fixed.len() == self.columns.len()
                && fixed
                    .iter()
                    .zip(&self.columns)
                    .all(|(fixed, column)| fixed.name == column.name && fixed.kind == column.kind);
            if !same {
                return Err(ResultError::FixedColumns);
            }
        }
        for row in &self.rows {
            if row.len() != self.columns.len() {
                return Err(ResultError::RowWidth);
            }
            if !row
                .iter()
                .zip(&self.columns)
                .all(|(cell, column)| column.kind.admits(cell))
            {
                return Err(ResultError::CellKind);
            }
        }
        if u32::try_from(self.rows.len()).ok() != Some(self.row_count) {
            return Err(ResultError::RowCount);
        }
        if self.row_count > max_rows.min(MAX_DATASET_ROWS) {
            return Err(ResultError::TooManyRows);
        }
        Ok(())
    }

    /// The cells of `column` in row order, for [`super::formula::Formula::evaluate`].
    #[must_use]
    pub fn column_cells(&self, column: &str) -> Option<Vec<super::formula::Cell>> {
        let index = self.columns.iter().position(|c| c.name == column)?;
        Some(
            self.rows
                .iter()
                .map(|row| {
                    row.get(index)
                        .map_or(super::formula::Cell::Null, super::formula::Cell::of_json)
                })
                .collect(),
        )
    }
}

/// The results of one check or one page view, for evaluating formulas.
impl super::formula::FormulaData for [DatasetResult] {
    fn cells(&self, dataset: &str, column: &str) -> Option<Vec<super::formula::Cell>> {
        self.iter()
            .find(|result| result.dataset == dataset)?
            .column_cells(column)
    }
}

/// Whether `text` is a time cell in one of the three forms of the module documentation.
#[must_use]
pub fn is_time_cell(text: &str) -> bool {
    const DATE: &[u8] = b"dddd-dd-dd";
    const DATE_TIME: &[u8] = b"dddd-dd-dd dd:dd:dd";
    const RFC3339: &[u8] = b"dddd-dd-ddTdd:dd:dd";
    let shaped = |prefix: &[u8]| {
        text.len() >= prefix.len()
            && text.bytes().zip(prefix).all(|(byte, shape)| match shape {
                b'd' => byte.is_ascii_digit(),
                other => byte == *other,
            })
    };
    // A fraction: a point and 1 to 9 digits.
    let fraction = |rest: &str| -> Option<usize> {
        let digits = rest.strip_prefix('.')?;
        let count = digits.bytes().take_while(u8::is_ascii_digit).count();
        (1..=9).contains(&count).then_some(count + 1)
    };
    if text.len() == DATE.len() && shaped(DATE) {
        return NaiveDate::parse_from_str(text, "%Y-%m-%d").is_ok();
    }
    if shaped(DATE_TIME) {
        let rest = text.get(DATE_TIME.len()..).unwrap_or_default();
        let complete = rest.is_empty() || fraction(rest) == Some(rest.len());
        return complete && NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S%.f").is_ok();
    }
    if shaped(RFC3339) {
        let rest = text.get(RFC3339.len()..).unwrap_or_default();
        let offset = rest.get(fraction(rest).unwrap_or(0)..).unwrap_or_default();
        let offset_shaped = offset == "Z"
            || (offset.len() == 6
                && offset.starts_with(['+', '-'])
                && offset
                    .bytes()
                    .skip(1)
                    .zip(b"dd:dd")
                    .all(|(byte, shape)| match shape {
                        b'd' => byte.is_ascii_digit(),
                        other => byte == *other,
                    }));
        return offset_shaped && DateTime::parse_from_rfc3339(text).is_ok();
    }
    false
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn sql_result(columns: &[(&str, ColumnKind)], rows: Vec<Vec<Value>>) -> DatasetResult {
        DatasetResult {
            dataset: "daily".to_owned(),
            columns: columns
                .iter()
                .map(|(name, kind)| ResultColumn {
                    name: (*name).to_owned(),
                    kind: *kind,
                })
                .collect(),
            row_count: u32::try_from(rows.len()).expect("few rows"),
            rows,
            meta: ResultMeta {
                queried_at: DateTime::parse_from_rfc3339("2026-10-26T01:02:03Z").expect("time"),
                duration_ms: 42,
                source: SourceMeta::Sql {
                    queryable_from: QueryablePeriod {
                        tenant_telemetry: DateTime::<Utc>::MIN_UTC,
                        tenant_telemetry_1m: DateTime::<Utc>::MIN_UTC,
                        tenant_telemetry_1h: DateTime::<Utc>::MIN_UTC,
                    },
                },
            },
        }
    }

    #[test]
    fn clickhouse_types_map_to_kinds() {
        for (data_type, kind) in [
            ("Float64", Some(Number)),
            ("Nullable(Float64)", Some(Number)),
            ("UInt64", Some(Number)),
            ("Decimal(18, 4)", Some(Number)),
            ("LowCardinality(String)", Some(Text)),
            ("LowCardinality(Nullable(String))", Some(Text)),
            ("FixedString(16)", Some(Text)),
            ("UUID", Some(Text)),
            ("Enum8('a' = 1, 'b' = 2)", Some(Text)),
            ("Date", Some(Time)),
            ("DateTime('Asia/Taipei')", Some(Time)),
            ("DateTime64(3, 'UTC')", Some(Time)),
            ("Nullable(DateTime64(3))", Some(Time)),
            ("Bool", Some(Bool)),
            ("Int128", None),
            ("UInt256", None),
            ("Array(Float64)", None),
            ("Tuple(Float64, String)", None),
            ("Map(String, Float64)", None),
            ("Nullable(Nothing)", None),
            ("Nullable(Float64", None),
            ("", None),
        ] {
            assert_eq!(
                ColumnKind::from_clickhouse_type(data_type),
                kind,
                "{data_type}"
            );
        }
    }

    #[test]
    fn time_cells_take_three_forms() {
        for text in [
            "2026-10-20",
            "2026-10-20 13:05:00",
            "2026-10-20 13:05:00.5",
            "2026-10-20 13:05:00.123456789",
            "2026-10-20T13:05:00+08:00",
            "2026-10-20T13:05:00.250-03:30",
            "2026-10-20T13:05:00Z",
        ] {
            assert!(is_time_cell(text), "{text}");
        }
        for text in [
            "",
            "2026-1-5",
            "2026-02-30",
            "2026-10-20 25:00:00",
            "2026-10-20 13:05",
            "2026-10-20 13:05:00.",
            "2026-10-20 13:05:00.1234567890",
            "2026-10-20 13:05:00Z",
            "2026-10-20T13:05:00",
            "2026-10-20T13:05:00+0800",
            "2026-10-20t13:05:00Z",
            "20261020",
            "1729400000",
        ] {
            assert!(!is_time_cell(text), "{text:?}");
        }
    }

    #[test]
    fn cells_must_match_their_column() {
        assert!(Number.admits(&json!(1.5)));
        assert!(Number.admits(&Value::Null));
        assert!(!Number.admits(&json!("1.5")));
        assert!(Text.admits(&json!("a")));
        assert!(!Text.admits(&json!(1)));
        assert!(Bool.admits(&json!(false)));
        assert!(!Bool.admits(&json!(0)));
        assert!(Time.admits(&json!("2026-10-20")));
        assert!(!Time.admits(&json!("yesterday")));
        assert!(!Time.admits(&json!(1_729_400_000)));
    }

    #[test]
    fn a_result_is_validated() {
        let good = sql_result(
            &[("day", Time), ("kwh", Number)],
            vec![
                vec![json!("2026-10-20"), json!(12.5)],
                vec![json!("2026-10-21"), Value::Null],
            ],
        );
        assert_eq!(good.validate(2), Ok(()));
        assert_eq!(good.validate(1), Err(ResultError::TooManyRows));

        let mut bad = good.clone();
        bad.dataset = "Daily".to_owned();
        assert_eq!(bad.validate(10), Err(ResultError::DatasetName));
        let mut bad = good.clone();
        bad.columns.clear();
        bad.rows.clear();
        bad.row_count = 0;
        assert_eq!(bad.validate(10), Err(ResultError::ColumnCount));
        let mut bad = good.clone();
        bad.columns[1].name = "day".to_owned();
        assert_eq!(bad.validate(10), Err(ResultError::ColumnName));
        let mut bad = good.clone();
        bad.columns[1].name = "sum(kwh)".to_owned();
        assert_eq!(bad.validate(10), Err(ResultError::ColumnName));
        let mut bad = good.clone();
        bad.rows[0].pop();
        assert_eq!(bad.validate(10), Err(ResultError::RowWidth));
        let mut bad = good.clone();
        bad.rows[0][1] = json!("12.5");
        assert_eq!(bad.validate(10), Err(ResultError::CellKind));
        let mut bad = good.clone();
        bad.row_count = 3;
        assert_eq!(bad.validate(10), Err(ResultError::RowCount));

        let mut semantic = good;
        semantic.meta.source = SourceMeta::TelemetryUsage {
            range: SemanticRange {
                tz: "Asia/Taipei".to_owned(),
                interval: SemanticInterval::Day1,
                from: DateTime::parse_from_rfc3339("2026-10-20T00:00:00+08:00").expect("time"),
                to: DateTime::parse_from_rfc3339("2026-10-27T00:00:00+08:00").expect("time"),
                truncated: false,
                earliest: DateTime::parse_from_rfc3339("2026-01-01T00:00:00+08:00").expect("time"),
                ends_after_now: true,
                resolution: SemanticResolution::Minute,
                queried_at: DateTime::parse_from_rfc3339("2026-10-26T09:00:00+08:00")
                    .expect("time"),
            },
        };
        assert_eq!(semantic.validate(10), Err(ResultError::FixedColumns));
        semantic.columns = USAGE_BUCKET_COLUMNS
            .iter()
            .map(|fixed| ResultColumn {
                name: fixed.name.to_owned(),
                kind: fixed.kind,
            })
            .collect();
        semantic.rows = vec![vec![
            json!("0190a5b2-7c3d-7e4f-8a1b-2c3d4e5f6a7b"),
            json!("energy"),
            json!("2026-10-20T00:00:00+08:00"),
            json!(12.5),
            json!(96),
            json!(0),
            json!(false),
            json!(0),
        ]];
        semantic.row_count = 1;
        assert_eq!(semantic.validate(10), Ok(()));
    }

    #[test]
    fn fixed_columns_are_valid_and_distinct() {
        for columns in [
            &STATISTICS_BUCKET_COLUMNS[..],
            &STATISTICS_TOTAL_COLUMNS,
            &USAGE_BUCKET_COLUMNS,
            &USAGE_TOTAL_COLUMNS,
        ] {
            let names: BTreeSet<&str> = columns.iter().map(|column| column.name).collect();
            assert_eq!(names.len(), columns.len());
            assert!(names.iter().all(|name| is_column_name(name)));
        }
        assert_eq!(
            semantic_columns(SourceKind::TelemetryStatistics, SemanticInterval::Hour1),
            Some(&STATISTICS_BUCKET_COLUMNS[..])
        );
        assert_eq!(
            semantic_columns(SourceKind::TelemetryUsage, SemanticInterval::Total),
            Some(&USAGE_TOTAL_COLUMNS[..])
        );
        assert_eq!(
            semantic_columns(SourceKind::Sql, SemanticInterval::Total),
            None
        );
    }
}
