//! The spec of a dashboard version: its datasets, computations, constants and component list,
//! and the checks that need no I/O.
//!
//! The `dashboard` tool writes a spec (design record 「資料模型」); the assistant validates it
//! with [`validate_version`] before it runs anything, stores it with the version, and gives the
//! page its public part ([`PublicSpec`]). Every type here refuses fields it does not know: the
//! model writes them. The components are in [`super::component`].
//!
//! Datasets, computations and constants share one namespace of [`is_name`] names, so a
//! reference or a formula names each without saying which kind it is.
//!
//! A version is a spec and an optional page. The page is the custom page, and the component
//! list has its item exactly when there is one. A version written before 2026-11-01 has no
//! component list and always a page: it stays valid and reads as a list of one custom page
//! (`Spec::layout`).

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, FixedOffset};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::component::{Component, validate_components, validate_page_item};
use super::formula::{DatasetShape, Formula, FormulaError, FormulaErrorClass};
use super::names::{COLUMN_NAME_PATTERN, NAME_PATTERN, is_name};
use super::result::{DatasetResult, MAX_DATASET_ROWS, semantic_columns};
use crate::DeviceId;
use crate::api::telemetry_query::MAX_SQL_BYTES;
use crate::error::MessageKey;

/// The most datasets of one version (decision of 2026-10-26): each runs at every view, and the
/// tenant's runs are capped per minute.
pub const MAX_DATASETS: usize = 8;

/// The most computations of one version.
pub const MAX_COMPUTATIONS: usize = 32;

/// The most constants of one version.
pub const MAX_CONSTANTS: usize = 32;

/// The largest page, in bytes of UTF-8 (decision of 2026-10-26).
pub const MAX_PAGE_BYTES: usize = 40 * 1024;

/// The largest version content, in bytes: the spec as compact JSON plus the page (decision of
/// 2026-10-26). The model reads it back whole to change it.
pub const MAX_CONTENT_BYTES: usize = 56 * 1024;

/// Longest dashboard, dataset, computation, constant or component title, in characters.
pub const MAX_TITLE_CHARS: usize = 120;

/// Longest description of a dashboard or a dataset, in characters: a sentence or two on what
/// the data is and how it is computed.
pub const MAX_DESCRIPTION_CHARS: usize = 500;

/// Longest unit of a computation, a constant or a component, in characters (`kWh`, `°C`,
/// `NT$/kWh`).
pub const MAX_UNIT_CHARS: usize = 24;

/// Longest note of a constant, in characters: where the figure comes from.
pub const MAX_NOTE_CHARS: usize = 300;

/// Longest text value of a constant, in characters.
pub const MAX_TEXT_VALUE_CHARS: usize = 200;

/// The most series of one semantic request (telemetry-query's `MAX_SERIES`).
pub const MAX_SERIES: usize = 20;

/// Longest data point key (telemetry-query's `MAX_POINT_LEN`).
pub const MAX_POINT_LEN: usize = 128;

/// Longest time zone name.
pub const MAX_TIME_ZONE_LEN: usize = 64;

/// The kind of a dataset's source.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
#[schema(as = DashboardSourceKind)]
pub enum SourceKind {
    /// Read-only SQL through telemetry-query's SQL endpoint (Laysi/trustiot-v2#143).
    Sql,
    /// `POST /api/telemetry/v1/statistics` (Laysi/trustiot-v2#142).
    TelemetryStatistics,
    /// `POST /api/telemetry/v1/usage` (Laysi/trustiot-v2#142).
    TelemetryUsage,
}

/// The bucket length of a semantic request: telemetry-query's `BucketInterval`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[schema(as = DashboardSemanticInterval)]
pub enum SemanticInterval {
    #[serde(rename = "1m")]
    Minute1,
    #[serde(rename = "5m")]
    Minutes5,
    #[serde(rename = "15m")]
    Minutes15,
    #[serde(rename = "1h")]
    Hour1,
    #[serde(rename = "1d")]
    Day1,
    #[serde(rename = "1w")]
    Week1,
    #[serde(rename = "1mo")]
    Month1,
    /// One bucket for the whole range.
    #[default]
    #[serde(rename = "total")]
    Total,
}

/// One data point of one device in a semantic request.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
#[schema(as = DashboardSemanticSeries)]
pub struct SemanticSeries {
    pub device_id: DeviceId,
    /// The data point key: 1 to 128 letters, digits, `.`, `_` or `-`, starting with a letter or
    /// digit.
    #[schema(pattern = "^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$")]
    pub point: String,
}

/// The request body of telemetry-query's statistics and usage APIs (Laysi/trustiot-v2#142),
/// stored as the model wrote it and sent unchanged at every run, so a relative `period` such as
/// `last_7_days` always reads the latest range. `from` and `to` keep the offset they were
/// written with. `services/telemetry-query/tests` checks that this type and the API's own
/// request type accept the same bodies.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
#[schema(as = DashboardSemanticRequest)]
pub struct SemanticRequest {
    /// 1 to [`MAX_SERIES`] distinct series.
    pub series: Vec<SemanticSeries>,
    /// An IANA time zone name, such as `Asia/Taipei`. Only its form is checked here; the API
    /// checks the name when the dataset first runs.
    pub tz: String,
    /// A relative period: `today`, `yesterday`, `this_week`, `last_week`, `this_month`,
    /// `last_month`, `this_year`, `last_year`, `last_<N>_days` (1 to 366) or `last_<N>_hours`
    /// (1 to 72). Either this or `from` and `to`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub period: Option<String>,
    /// The range's start, RFC 3339 with an offset (inclusive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<DateTime<FixedOffset>>,
    /// The range's end, RFC 3339 with an offset (exclusive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<DateTime<FixedOffset>>,
    #[serde(default)]
    pub interval: SemanticInterval,
}

/// Where a dataset's rows come from.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
#[schema(as = DashboardDatasetSource)]
pub enum DatasetSource {
    /// One read-only `SELECT` over the tenant's telemetry views, at most telemetry-query's
    /// `MAX_SQL_BYTES`. Every column needs a dashboard column name: name computed ones with
    /// `AS`.
    Sql { sql: String },
    /// A statistics request.
    TelemetryStatistics { request: SemanticRequest },
    /// A usage request.
    TelemetryUsage { request: SemanticRequest },
}

impl DatasetSource {
    /// The source's kind.
    #[must_use]
    pub const fn kind(&self) -> SourceKind {
        match self {
            Self::Sql { .. } => SourceKind::Sql,
            Self::TelemetryStatistics { .. } => SourceKind::TelemetryStatistics,
            Self::TelemetryUsage { .. } => SourceKind::TelemetryUsage,
        }
    }
}

/// A named query whose result the page shows.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
#[schema(as = DashboardDataset)]
pub struct Dataset {
    #[schema(pattern = "^[a-z][a-z0-9_]{0,47}$")]
    pub name: String,
    /// 1 to [`MAX_TITLE_CHARS`] characters.
    pub title: String,
    /// What the data is and how it is computed, 1 to [`MAX_DESCRIPTION_CHARS`] characters.
    pub description: String,
    /// The most rows the dataset may return, 1 to [`MAX_DATASET_ROWS`]. A run that returns more
    /// is refused, never cut short.
    #[schema(minimum = 1, maximum = 2000)]
    pub max_rows: u32,
    pub source: DatasetSource,
}

/// A named number computed from the datasets' results with a formula
/// ([`super::formula`]).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
#[schema(as = DashboardComputation)]
pub struct Computation {
    #[schema(pattern = "^[a-z][a-z0-9_]{0,47}$")]
    pub name: String,
    pub title: String,
    /// At most `formula::MAX_FORMULA_BYTES` bytes.
    pub formula: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
}

/// A constant's value.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(untagged)]
#[schema(as = DashboardConstantValue)]
pub enum ConstantValue {
    Number(f64),
    Text(String),
}

/// A named fixed value the page shows, with where it comes from: a target, a tariff, an
/// emission factor, or text with digits in it (a line's name, a standard's number) that the
/// page must not write as a literal.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
#[schema(as = DashboardConstant)]
pub struct Constant {
    #[schema(pattern = "^[a-z][a-z0-9_]{0,47}$")]
    pub name: String,
    pub title: String,
    pub value: ConstantValue,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    /// Where the value comes from, at most [`MAX_NOTE_CHARS`] characters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// A version's datasets, computations, constants and component list.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
#[schema(as = DashboardSpec)]
pub struct Spec {
    /// 1 to [`MAX_DATASETS`].
    pub datasets: Vec<Dataset>,
    /// At most [`MAX_COMPUTATIONS`].
    #[serde(default)]
    pub computations: Vec<Computation>,
    /// At most [`MAX_CONSTANTS`].
    #[serde(default)]
    pub constants: Vec<Constant>,
    /// What the screen shows, in order: up to `component::MAX_COMPONENTS` items, at most one of
    /// them the custom page. Empty only in a version written before 2026-11-01, whose page is
    /// its only item (`Spec::layout`); left out of the JSON when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub components: Vec<Component>,
}

/// The part of a dataset the page and the check see: no SQL and no request.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PublicDataset {
    pub name: String,
    pub title: String,
    pub description: String,
    pub source: SourceKind,
    pub max_rows: u32,
}

/// The part of a spec the page, the check and the components see: the datasets without their
/// queries, and every computation, constant and component. Internal callers accept fields they
/// do not know; `components` is left out when empty, so a version without one gives the same
/// JSON as before 2026-11-01.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PublicSpec {
    pub datasets: Vec<PublicDataset>,
    pub computations: Vec<Computation>,
    pub constants: Vec<Constant>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub components: Vec<Component>,
}

/// What is wrong with one part of a version, for the model: [`SpecError::path`] is a JSON
/// pointer into the `dashboard` tool's input (`/spec/datasets/2/name`, `/page`), and every
/// message is fixed English text.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SpecError {
    pub path: String,
    #[serde(flatten)]
    pub problem: SpecProblem,
}

impl SpecError {
    pub(super) fn new(path: impl Into<String>, problem: SpecProblem) -> Self {
        Self {
            path: path.into(),
            problem,
        }
    }
}

/// The closed set of [`SpecError`] problems. Each has a fixed English message for the model
/// ([`Self::message`]) and a message key for the user ([`Self::message_key`]); the key's details
/// are the problem's other fields (`max`, `max_chars`, `max_bytes`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "problem", rename_all = "snake_case")]
pub enum SpecProblem {
    NoDatasets,
    TooManyDatasets {
        max: usize,
    },
    TooManyComputations {
        max: usize,
    },
    TooManyConstants {
        max: usize,
    },
    NameInvalid,
    NameDuplicate,
    TitleInvalid {
        max_chars: usize,
    },
    DescriptionInvalid {
        max_chars: usize,
    },
    UnitInvalid {
        max_chars: usize,
    },
    NoteInvalid {
        max_chars: usize,
    },
    TextValueInvalid {
        max_chars: usize,
    },
    NumberNotFinite,
    MaxRowsOutOfRange {
        max: u32,
    },
    SqlEmpty,
    SqlTooLong {
        max_bytes: u32,
    },
    SeriesCount {
        max: usize,
    },
    SeriesDuplicate,
    PointInvalid,
    TimeZoneInvalid,
    RangeInvalid,
    PeriodInvalid,
    Formula(FormulaError),
    PageEmpty,
    PageTooLarge {
        max_bytes: usize,
    },
    ContentTooLarge {
        max_bytes: usize,
    },
    /// No components and no page.
    NoComponents,
    TooManyComponents {
        max: usize,
    },
    /// A second custom page item.
    CustomPageDuplicate,
    /// A custom page item in a version without a page.
    CustomPageWithoutPage,
    /// A page in a version whose component list has no custom page item.
    PageWithoutCustomPage,
    /// A version with a custom page where custom pages are not enabled: the assistant has no
    /// check service (`dashboard_check.url`), which every custom page must pass before it is
    /// stored (Laysi/trustiot-v2#934). [`validate_version`] never reports it; the assistant
    /// does, for the whole version, before anything runs.
    CustomPageDisabled,
    /// A component that is not a JSON object, or does not read as one.
    ComponentInvalid,
    /// A component's `kind` is missing or not one of `component::ComponentKind`.
    ComponentKindUnknown,
    ComponentFieldUnknown,
    ComponentFieldMissing,
    /// A field of the wrong JSON form (a number for a name, an object for a list).
    ComponentFieldInvalid,
    /// A component names a dataset the spec does not declare (or, with results, one without a
    /// result).
    DatasetUnknown,
    /// A component names a computation the spec does not declare.
    ComputationUnknown,
    /// A threshold names a constant the spec does not declare.
    ConstantUnknown,
    /// A column name that breaks the column name rules.
    ColumnInvalid,
    /// A component names one column twice (two `y`, `x` again as `y` or as the series).
    ColumnDuplicate,
    YColumnsCount {
        max: usize,
    },
    TableColumnsCount {
        max: usize,
    },
    ThresholdsCount {
        max: usize,
    },
    /// A threshold names a text constant.
    ThresholdNotNumber,
    /// Thresholds that are not strictly increasing.
    ThresholdsNotAscending,
    /// A cell's row at or past its dataset's `max_rows`.
    RowOutOfRange,
    /// A cell's row past the end of this run's result: the component would show nothing, as a
    /// custom page's reference past the end is refused by the check.
    RowMissing,
    /// The dataset's result has no such column.
    ColumnUnknown,
    ColumnNotTime,
    ColumnNotNumber,
    ColumnNotText,
    /// A bar chart's x is neither a text nor a time column.
    ColumnNotCategory,
    /// Two rows of one series share one x.
    XDuplicate,
    /// A chart's `y` columns times its series column's distinct values in the result are more
    /// than `component::MAX_SERIES`.
    SeriesTooMany {
        max: usize,
    },
}

impl SpecProblem {
    /// The fixed English message.
    #[must_use]
    pub fn message(self) -> String {
        match self {
            Self::NoDatasets => "A dashboard needs at least one dataset.".to_owned(),
            Self::TooManyDatasets { max } => format!("A dashboard has at most {max} datasets."),
            Self::TooManyComputations { max } => {
                format!("A dashboard has at most {max} computations.")
            }
            Self::TooManyConstants { max } => format!("A dashboard has at most {max} constants."),
            Self::NameInvalid => format!(
                "A name is a lowercase letter followed by lowercase letters, digits and _ \
                 ({NAME_PATTERN})."
            ),
            Self::NameDuplicate => {
                "Datasets, computations and constants share one set of names; this one is \
                 already used."
                    .to_owned()
            }
            Self::TitleInvalid { max_chars } => format!(
                "A title is 1 to {max_chars} characters without control or invisible characters."
            ),
            Self::DescriptionInvalid { max_chars } => format!(
                "A description is 1 to {max_chars} characters without control or invisible \
                 characters."
            ),
            Self::UnitInvalid { max_chars } => format!(
                "A unit is 1 to {max_chars} characters without control or invisible characters."
            ),
            Self::NoteInvalid { max_chars } => format!(
                "A note is 1 to {max_chars} characters without control or invisible characters."
            ),
            Self::TextValueInvalid { max_chars } => format!(
                "A text value is 1 to {max_chars} characters without control or invisible \
                 characters."
            ),
            Self::NumberNotFinite => "A number must be finite.".to_owned(),
            Self::MaxRowsOutOfRange { max } => format!(
                "max_rows is 1 to {max}; aggregate in the query or narrow the range to stay \
                 within it."
            ),
            Self::SqlEmpty => "The SQL is empty.".to_owned(),
            Self::SqlTooLong { max_bytes } => {
                format!("The SQL is longer than {max_bytes} bytes.")
            }
            Self::SeriesCount { max } => {
                format!("A semantic request names 1 to {max} series.")
            }
            Self::SeriesDuplicate => "A semantic request names a series twice.".to_owned(),
            Self::PointInvalid => {
                "A data point key is 1 to 128 letters, digits, '.', '_' or '-', starting with a \
                 letter or digit."
                    .to_owned()
            }
            Self::TimeZoneInvalid => {
                "The time zone is not an IANA time zone name such as Asia/Taipei.".to_owned()
            }
            Self::RangeInvalid => {
                "Give either period, or from and to with from before to.".to_owned()
            }
            Self::PeriodInvalid => {
                "The period is not one of today, yesterday, this_week, last_week, this_month, \
                 last_month, this_year, last_year, last_<N>_days (1 to 366) or last_<N>_hours \
                 (1 to 72)."
                    .to_owned()
            }
            Self::Formula(error) => error.to_string(),
            Self::PageEmpty => "The page is empty.".to_owned(),
            Self::PageTooLarge { max_bytes } => {
                format!("The page is larger than {max_bytes} bytes.")
            }
            Self::ContentTooLarge { max_bytes } => {
                format!("The spec as JSON and the page together are larger than {max_bytes} bytes.")
            }
            Self::NoComponents => {
                "A dashboard needs at least one component, or a custom page.".to_owned()
            }
            Self::TooManyComponents { max } => {
                format!("A dashboard has at most {max} components, the custom page included.")
            }
            Self::CustomPageDuplicate => "A dashboard has at most one custom page.".to_owned(),
            Self::CustomPageWithoutPage => {
                "The components list a custom page, but there is no page; write the page or \
                 remove the custom_page item."
                    .to_owned()
            }
            Self::PageWithoutCustomPage => {
                "There is a page, but no custom_page item in the components says where it goes."
                    .to_owned()
            }
            Self::CustomPageDisabled => {
                "Custom pages are not enabled in this environment: a dashboard here shows \
                 components only, without a page. Nothing was saved."
                    .to_owned()
            }
            Self::ComponentInvalid => {
                "A component is an object with a kind and that kind's fields.".to_owned()
            }
            Self::ComponentKindUnknown => {
                "A component's kind is one of time_series, bar, kpi, table, status or \
                 custom_page."
                    .to_owned()
            }
            Self::ComponentFieldUnknown => {
                "This component kind has no such field; components take no colors, styles, HTML \
                 or URLs."
                    .to_owned()
            }
            Self::ComponentFieldMissing => "This component kind needs this field.".to_owned(),
            Self::ComponentFieldInvalid => {
                "This field has the wrong form: names and titles are strings, y and thresholds \
                 are lists of names, a value is a cell or a computation."
                    .to_owned()
            }
            Self::DatasetUnknown => "No dataset of the dashboard has this name.".to_owned(),
            Self::ComputationUnknown => "No computation of the dashboard has this name.".to_owned(),
            Self::ConstantUnknown => "No constant of the dashboard has this name.".to_owned(),
            Self::ColumnInvalid => format!(
                "A column name is a letter or _ followed by letters, digits and _ \
                 ({COLUMN_NAME_PATTERN})."
            ),
            Self::ColumnDuplicate => "A component names this column twice.".to_owned(),
            Self::YColumnsCount { max } => format!("A chart has 1 to {max} y columns."),
            Self::TableColumnsCount { max } => format!("A table has 1 to {max} columns."),
            Self::ThresholdsCount { max } => {
                format!("A status has 1 to {max} thresholds, a chart at most {max}.")
            }
            Self::ThresholdNotNumber => "A threshold is a number constant.".to_owned(),
            Self::ThresholdsNotAscending => {
                "Thresholds go from low to high, each larger than the one before.".to_owned()
            }
            Self::RowOutOfRange => {
                "The row is a 0-based index below the dataset's max_rows.".to_owned()
            }
            Self::RowMissing => {
                "The dataset returned fewer rows this time, so this row is empty; use a row it \
                 always returns, such as row 0 of an aggregate."
                    .to_owned()
            }
            Self::ColumnUnknown => "The dataset's result has no column of this name.".to_owned(),
            Self::ColumnNotTime => {
                "This column must hold times (Date or DateTime in SQL).".to_owned()
            }
            Self::ColumnNotNumber => "This column must hold numbers.".to_owned(),
            Self::ColumnNotText => "This column must hold text.".to_owned(),
            Self::ColumnNotCategory => {
                "A bar chart's x must hold text or times; convert numbers with toString.".to_owned()
            }
            Self::XDuplicate => {
                "Two rows of one series have the same x; combine them in the query (GROUP BY) so \
                 each point is one row."
                    .to_owned()
            }
            Self::SeriesTooMany { max } => format!(
                "This chart would draw more than {max} series: its y columns times the series \
                 column's distinct values. Keep fewer series in the query, or split the chart."
            ),
        }
    }

    /// The message key for the user: `dashboard.spec.` and the problem's name.
    #[must_use]
    pub const fn message_key(self) -> MessageKey {
        match self {
            Self::NoDatasets => MessageKey::new("dashboard.spec.no_datasets"),
            Self::TooManyDatasets { .. } => MessageKey::new("dashboard.spec.too_many_datasets"),
            Self::TooManyComputations { .. } => {
                MessageKey::new("dashboard.spec.too_many_computations")
            }
            Self::TooManyConstants { .. } => MessageKey::new("dashboard.spec.too_many_constants"),
            Self::NameInvalid => MessageKey::new("dashboard.spec.name_invalid"),
            Self::NameDuplicate => MessageKey::new("dashboard.spec.name_duplicate"),
            Self::TitleInvalid { .. } => MessageKey::new("dashboard.spec.title_invalid"),
            Self::DescriptionInvalid { .. } => {
                MessageKey::new("dashboard.spec.description_invalid")
            }
            Self::UnitInvalid { .. } => MessageKey::new("dashboard.spec.unit_invalid"),
            Self::NoteInvalid { .. } => MessageKey::new("dashboard.spec.note_invalid"),
            Self::TextValueInvalid { .. } => MessageKey::new("dashboard.spec.text_value_invalid"),
            Self::NumberNotFinite => MessageKey::new("dashboard.spec.number_not_finite"),
            Self::MaxRowsOutOfRange { .. } => {
                MessageKey::new("dashboard.spec.max_rows_out_of_range")
            }
            Self::SqlEmpty => MessageKey::new("dashboard.spec.sql_empty"),
            Self::SqlTooLong { .. } => MessageKey::new("dashboard.spec.sql_too_long"),
            Self::SeriesCount { .. } => MessageKey::new("dashboard.spec.series_count"),
            Self::SeriesDuplicate => MessageKey::new("dashboard.spec.series_duplicate"),
            Self::PointInvalid => MessageKey::new("dashboard.spec.point_invalid"),
            Self::TimeZoneInvalid => MessageKey::new("dashboard.spec.time_zone_invalid"),
            Self::RangeInvalid => MessageKey::new("dashboard.spec.range_invalid"),
            Self::PeriodInvalid => MessageKey::new("dashboard.spec.period_invalid"),
            Self::Formula(_) => MessageKey::new("dashboard.spec.formula"),
            Self::PageEmpty => MessageKey::new("dashboard.spec.page_empty"),
            Self::PageTooLarge { .. } => MessageKey::new("dashboard.spec.page_too_large"),
            Self::ContentTooLarge { .. } => MessageKey::new("dashboard.spec.content_too_large"),
            Self::NoComponents => MessageKey::new("dashboard.spec.no_components"),
            Self::TooManyComponents { .. } => MessageKey::new("dashboard.spec.too_many_components"),
            Self::CustomPageDuplicate => MessageKey::new("dashboard.spec.custom_page_duplicate"),
            Self::CustomPageWithoutPage => {
                MessageKey::new("dashboard.spec.custom_page_without_page")
            }
            Self::PageWithoutCustomPage => {
                MessageKey::new("dashboard.spec.page_without_custom_page")
            }
            Self::CustomPageDisabled => MessageKey::new("dashboard.spec.custom_page_disabled"),
            Self::ComponentInvalid => MessageKey::new("dashboard.spec.component_invalid"),
            Self::ComponentKindUnknown => MessageKey::new("dashboard.spec.component_kind_unknown"),
            Self::ComponentFieldUnknown => {
                MessageKey::new("dashboard.spec.component_field_unknown")
            }
            Self::ComponentFieldMissing => {
                MessageKey::new("dashboard.spec.component_field_missing")
            }
            Self::ComponentFieldInvalid => {
                MessageKey::new("dashboard.spec.component_field_invalid")
            }
            Self::DatasetUnknown => MessageKey::new("dashboard.spec.dataset_unknown"),
            Self::ComputationUnknown => MessageKey::new("dashboard.spec.computation_unknown"),
            Self::ConstantUnknown => MessageKey::new("dashboard.spec.constant_unknown"),
            Self::ColumnInvalid => MessageKey::new("dashboard.spec.column_invalid"),
            Self::ColumnDuplicate => MessageKey::new("dashboard.spec.column_duplicate"),
            Self::YColumnsCount { .. } => MessageKey::new("dashboard.spec.y_columns_count"),
            Self::TableColumnsCount { .. } => MessageKey::new("dashboard.spec.table_columns_count"),
            Self::ThresholdsCount { .. } => MessageKey::new("dashboard.spec.thresholds_count"),
            Self::ThresholdNotNumber => MessageKey::new("dashboard.spec.threshold_not_number"),
            Self::ThresholdsNotAscending => {
                MessageKey::new("dashboard.spec.thresholds_not_ascending")
            }
            Self::RowOutOfRange => MessageKey::new("dashboard.spec.row_out_of_range"),
            Self::RowMissing => MessageKey::new("dashboard.spec.row_missing"),
            Self::ColumnUnknown => MessageKey::new("dashboard.spec.column_unknown"),
            Self::ColumnNotTime => MessageKey::new("dashboard.spec.column_not_time"),
            Self::ColumnNotNumber => MessageKey::new("dashboard.spec.column_not_number"),
            Self::ColumnNotText => MessageKey::new("dashboard.spec.column_not_text"),
            Self::ColumnNotCategory => MessageKey::new("dashboard.spec.column_not_category"),
            Self::XDuplicate => MessageKey::new("dashboard.spec.x_duplicate"),
            Self::SeriesTooMany { .. } => MessageKey::new("dashboard.spec.series_too_many"),
        }
    }
}

impl Spec {
    /// Checks everything that needs no I/O and no page: the counts, every name, title,
    /// description, unit, note and value, every source, every formula against the declared
    /// datasets (and, for semantic datasets, their fixed columns), and every component's
    /// references and limits. Paths start at `/spec`. Whether the list and the page agree is
    /// [`validate_version`]'s.
    #[must_use]
    pub fn validate(&self) -> Vec<SpecError> {
        let mut errors = Vec::new();
        if self.datasets.is_empty() {
            errors.push(SpecError::new("/spec/datasets", SpecProblem::NoDatasets));
        }
        if self.datasets.len() > MAX_DATASETS {
            errors.push(SpecError::new(
                "/spec/datasets",
                SpecProblem::TooManyDatasets { max: MAX_DATASETS },
            ));
        }
        if self.computations.len() > MAX_COMPUTATIONS {
            errors.push(SpecError::new(
                "/spec/computations",
                SpecProblem::TooManyComputations {
                    max: MAX_COMPUTATIONS,
                },
            ));
        }
        if self.constants.len() > MAX_CONSTANTS {
            errors.push(SpecError::new(
                "/spec/constants",
                SpecProblem::TooManyConstants { max: MAX_CONSTANTS },
            ));
        }

        let mut names = BTreeSet::new();
        let mut name = |errors: &mut Vec<SpecError>, path: String, name: &str| {
            if !is_name(name) {
                errors.push(SpecError::new(path, SpecProblem::NameInvalid));
            } else if !names.insert(name.to_owned()) {
                errors.push(SpecError::new(path, SpecProblem::NameDuplicate));
            }
        };
        for (index, dataset) in self.datasets.iter().enumerate() {
            let at = format!("/spec/datasets/{index}");
            name(&mut errors, format!("{at}/name"), &dataset.name);
            text(&mut errors, &at, "title", &dataset.title, TextRule::TITLE);
            text(
                &mut errors,
                &at,
                "description",
                &dataset.description,
                TextRule::DESCRIPTION,
            );
            if !(1..=MAX_DATASET_ROWS).contains(&dataset.max_rows) {
                errors.push(SpecError::new(
                    format!("{at}/max_rows"),
                    SpecProblem::MaxRowsOutOfRange {
                        max: MAX_DATASET_ROWS,
                    },
                ));
            }
            validate_source(&mut errors, &format!("{at}/source"), &dataset.source);
        }
        for (index, computation) in self.computations.iter().enumerate() {
            let at = format!("/spec/computations/{index}");
            name(&mut errors, format!("{at}/name"), &computation.name);
            text(
                &mut errors,
                &at,
                "title",
                &computation.title,
                TextRule::TITLE,
            );
            if let Some(unit) = &computation.unit {
                text(&mut errors, &at, "unit", unit, TextRule::UNIT);
            }
        }
        for (index, constant) in self.constants.iter().enumerate() {
            let at = format!("/spec/constants/{index}");
            name(&mut errors, format!("{at}/name"), &constant.name);
            text(&mut errors, &at, "title", &constant.title, TextRule::TITLE);
            match &constant.value {
                ConstantValue::Number(number) if !number.is_finite() => errors.push(
                    SpecError::new(format!("{at}/value"), SpecProblem::NumberNotFinite),
                ),
                ConstantValue::Number(_) => {}
                ConstantValue::Text(value) => {
                    text(&mut errors, &at, "value", value, TextRule::TEXT_VALUE);
                }
            }
            if let Some(unit) = &constant.unit {
                text(&mut errors, &at, "unit", unit, TextRule::UNIT);
            }
            if let Some(note) = &constant.note {
                text(&mut errors, &at, "note", note, TextRule::NOTE);
            }
        }
        errors.extend(self.check_formulas(&[]));
        validate_components(self, &mut errors);
        errors
    }

    /// Checks every formula against the declared datasets: semantic datasets by their fixed
    /// columns, SQL datasets by their columns in `results` (a trial run's) when one is there, and
    /// by their `max_rows` only otherwise.
    #[must_use]
    pub fn check_formulas(&self, results: &[DatasetResult]) -> Vec<SpecError> {
        let shapes: BTreeMap<&str, DatasetShape> = self
            .datasets
            .iter()
            .rev()
            .map(|dataset| (dataset.name.as_str(), self.shape(dataset, results)))
            .collect();
        let mut errors = Vec::new();
        for (index, computation) in self.computations.iter().enumerate() {
            let checked = Formula::parse(&computation.formula)
                .and_then(|formula| formula.check(|name| shapes.get(name).cloned()));
            if let Err(error) = checked {
                errors.push(SpecError::new(
                    format!("/spec/computations/{index}/formula"),
                    SpecProblem::Formula(error),
                ));
            }
        }
        errors
    }

    fn shape(&self, dataset: &Dataset, results: &[DatasetResult]) -> DatasetShape {
        let fixed = match &dataset.source {
            DatasetSource::Sql { .. } => None,
            DatasetSource::TelemetryStatistics { request }
            | DatasetSource::TelemetryUsage { request } => {
                semantic_columns(dataset.source.kind(), request.interval)
            }
        };
        let columns = fixed
            .map(|columns| {
                columns
                    .iter()
                    .map(|column| (column.name.to_owned(), column.kind))
                    .collect()
            })
            .or_else(|| {
                let result = results
                    .iter()
                    .find(|result| result.dataset == dataset.name)?;
                Some(
                    result
                        .columns
                        .iter()
                        .map(|column| (column.name.clone(), column.kind))
                        .collect(),
                )
            });
        DatasetShape {
            max_rows: dataset.max_rows.clamp(1, MAX_DATASET_ROWS),
            columns,
        }
    }

    /// The part the page and the check see.
    #[must_use]
    pub fn public(&self) -> PublicSpec {
        PublicSpec {
            datasets: self
                .datasets
                .iter()
                .map(|dataset| PublicDataset {
                    name: dataset.name.clone(),
                    title: dataset.title.clone(),
                    description: dataset.description.clone(),
                    source: dataset.source.kind(),
                    max_rows: dataset.max_rows,
                })
                .collect(),
            computations: self.computations.clone(),
            constants: self.constants.clone(),
            components: self.components.clone(),
        }
    }

    /// The dataset named `name`.
    #[must_use]
    pub fn dataset(&self, name: &str) -> Option<&Dataset> {
        self.datasets.iter().find(|dataset| dataset.name == name)
    }
}

/// Checks a whole version: the dashboard's title and optional description, the spec
/// ([`Spec::validate`]), the page when there is one (not empty, at most [`MAX_PAGE_BYTES`]),
/// that the component list and the page agree, and the content's size (at most
/// [`MAX_CONTENT_BYTES`]).
///
/// The page is optional since 2026-11-01: a version of components only has none. The list has
/// at least one item, at most one of them the custom page, and that item is there exactly when
/// the page is ([`SpecProblem::CustomPageWithoutPage`], [`SpecProblem::PageWithoutCustomPage`]).
/// A version written before then has no list and a page; it reads as a list of one custom page
/// ([`Spec::layout`]) and passes unchanged. Neither a list nor a page is
/// [`SpecProblem::NoComponents`].
#[must_use]
pub fn validate_version(
    title: &str,
    description: Option<&str>,
    spec: &Spec,
    page: Option<&str>,
) -> Vec<SpecError> {
    let mut errors = Vec::new();
    text(&mut errors, "", "title", title, TextRule::TITLE);
    if let Some(description) = description {
        text(
            &mut errors,
            "",
            "description",
            description,
            TextRule::DESCRIPTION,
        );
    }
    errors.extend(spec.validate());
    if let Some(page) = page {
        if page.trim().is_empty() {
            errors.push(SpecError::new("/page", SpecProblem::PageEmpty));
        } else if page.len() > MAX_PAGE_BYTES {
            errors.push(SpecError::new(
                "/page",
                SpecProblem::PageTooLarge {
                    max_bytes: MAX_PAGE_BYTES,
                },
            ));
        }
    }
    validate_page_item(spec, page.is_some(), &mut errors);
    let spec_bytes = serde_json::to_vec(spec).map_or(usize::MAX, |json| json.len());
    let page_bytes = page.map_or(0, str::len);
    if spec_bytes.saturating_add(page_bytes) > MAX_CONTENT_BYTES {
        errors.push(SpecError::new(
            "",
            SpecProblem::ContentTooLarge {
                max_bytes: MAX_CONTENT_BYTES,
            },
        ));
    }
    errors
}

#[derive(Clone, Copy)]
pub(super) struct TextRule {
    max_chars: usize,
    problem: fn(usize) -> SpecProblem,
}

impl TextRule {
    pub(super) const TITLE: Self = Self {
        max_chars: MAX_TITLE_CHARS,
        problem: |max_chars| SpecProblem::TitleInvalid { max_chars },
    };
    const DESCRIPTION: Self = Self {
        max_chars: MAX_DESCRIPTION_CHARS,
        problem: |max_chars| SpecProblem::DescriptionInvalid { max_chars },
    };
    pub(super) const UNIT: Self = Self {
        max_chars: MAX_UNIT_CHARS,
        problem: |max_chars| SpecProblem::UnitInvalid { max_chars },
    };
    const NOTE: Self = Self {
        max_chars: MAX_NOTE_CHARS,
        problem: |max_chars| SpecProblem::NoteInvalid { max_chars },
    };
    const TEXT_VALUE: Self = Self {
        max_chars: MAX_TEXT_VALUE_CHARS,
        problem: |max_chars| SpecProblem::TextValueInvalid { max_chars },
    };
}

/// Checks one text field: not blank, at most the rule's characters, and no control or
/// invisible formatting characters (which could hide or reorder what a user reads).
pub(super) fn text(
    errors: &mut Vec<SpecError>,
    at: &str,
    field: &str,
    value: &str,
    rule: TextRule,
) {
    let count = value.chars().count();
    let visible = !value.trim().is_empty() && !value.chars().any(is_hidden_char);
    if count > rule.max_chars || !visible {
        errors.push(SpecError::new(
            format!("{at}/{field}"),
            (rule.problem)(rule.max_chars),
        ));
    }
}

/// A control character, or a character that changes how text is shown without being seen:
/// zero-width characters, bidirectional controls, and the byte order mark.
pub(crate) fn is_hidden_char(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{00AD}'
                | '\u{061C}'
                | '\u{180E}'
                | '\u{200B}'..='\u{200F}'
                | '\u{202A}'..='\u{202E}'
                | '\u{2060}'..='\u{2064}'
                | '\u{2066}'..='\u{206F}'
                | '\u{FEFF}'
                | '\u{FFF9}'..='\u{FFFB}'
        )
}

fn validate_source(errors: &mut Vec<SpecError>, at: &str, source: &DatasetSource) {
    match source {
        DatasetSource::Sql { sql } => {
            if sql.trim().is_empty() {
                errors.push(SpecError::new(format!("{at}/sql"), SpecProblem::SqlEmpty));
            } else if !u32::try_from(sql.len()).is_ok_and(|len| len <= MAX_SQL_BYTES) {
                errors.push(SpecError::new(
                    format!("{at}/sql"),
                    SpecProblem::SqlTooLong {
                        max_bytes: MAX_SQL_BYTES,
                    },
                ));
            }
        }
        DatasetSource::TelemetryStatistics { request }
        | DatasetSource::TelemetryUsage { request } => {
            validate_request(errors, &format!("{at}/request"), request);
        }
    }
}

fn validate_request(errors: &mut Vec<SpecError>, at: &str, request: &SemanticRequest) {
    if request.series.is_empty() || request.series.len() > MAX_SERIES {
        errors.push(SpecError::new(
            format!("{at}/series"),
            SpecProblem::SeriesCount { max: MAX_SERIES },
        ));
    }
    let mut seen = BTreeSet::new();
    for (index, series) in request.series.iter().enumerate() {
        if !is_point_key(&series.point) {
            errors.push(SpecError::new(
                format!("{at}/series/{index}/point"),
                SpecProblem::PointInvalid,
            ));
        }
        if !seen.insert(series) {
            errors.push(SpecError::new(
                format!("{at}/series/{index}"),
                SpecProblem::SeriesDuplicate,
            ));
        }
    }
    if !is_time_zone_name(&request.tz) {
        errors.push(SpecError::new(
            format!("{at}/tz"),
            SpecProblem::TimeZoneInvalid,
        ));
    }
    match (&request.period, request.from, request.to) {
        (Some(period), None, None) => {
            if !is_period(period) {
                errors.push(SpecError::new(
                    format!("{at}/period"),
                    SpecProblem::PeriodInvalid,
                ));
            }
        }
        (None, Some(from), Some(to)) if from < to => {}
        _ => errors.push(SpecError::new(at.to_owned(), SpecProblem::RangeInvalid)),
    }
}

/// A data point key, as telemetry-query checks it.
fn is_point_key(point: &str) -> bool {
    let bytes = point.as_bytes();
    bytes.len() <= MAX_POINT_LEN
        && bytes.first().is_some_and(u8::is_ascii_alphanumeric)
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

/// The form of an IANA time zone name: `UTC`, `Asia/Taipei`, `America/Argentina/Buenos_Aires`,
/// `Etc/GMT+8`.
fn is_time_zone_name(tz: &str) -> bool {
    tz.len() <= MAX_TIME_ZONE_LEN
        && tz.split('/').all(|part| {
            part.as_bytes().first().is_some_and(u8::is_ascii_alphabetic)
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'+'))
        })
}

/// A relative period telemetry-query accepts.
fn is_period(period: &str) -> bool {
    const FIXED: [&str; 8] = [
        "today",
        "yesterday",
        "this_week",
        "last_week",
        "this_month",
        "last_month",
        "this_year",
        "last_year",
    ];
    if FIXED.contains(&period) {
        return true;
    }
    let count = |text: &str, max: u32| {
        text.bytes()
            .next()
            .is_some_and(|byte| matches!(byte, b'1'..=b'9'))
            && text.bytes().all(|byte| byte.is_ascii_digit())
            && text.len() <= 3
            && text.parse::<u32>().is_ok_and(|n| n <= max)
    };
    period
        .strip_prefix("last_")
        .is_some_and(|rest| match rest.rsplit_once('_') {
            Some((n, "days")) => count(n, 366),
            Some((n, "hours")) => count(n, 72),
            _ => false,
        })
}

impl SpecError {
    /// The fixed English message.
    #[must_use]
    pub fn message(&self) -> String {
        self.problem.message()
    }
}

impl From<FormulaError> for SpecProblem {
    fn from(error: FormulaError) -> Self {
        Self::Formula(error)
    }
}

impl SpecProblem {
    /// The formula error class, for a [`SpecProblem::Formula`].
    #[must_use]
    pub const fn formula_class(self) -> Option<FormulaErrorClass> {
        match self {
            Self::Formula(error) => Some(error.class),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    fn spec_json() -> Value {
        json!({
            "datasets": [
                {
                    "name": "daily",
                    "title": "Daily energy",
                    "description": "Energy per day over the last seven days, from the hourly cells.",
                    "max_rows": 31,
                    "source": {"kind": "sql", "sql": "SELECT 1 AS kwh"}
                },
                {
                    "name": "usage",
                    "title": "Usage this week",
                    "description": "The main meter's usage per day.",
                    "max_rows": 8,
                    "source": {
                        "kind": "telemetry_usage",
                        "request": {
                            "series": [{"device_id": "0190a5b2-7c3d-7e4f-8a1b-2c3d4e5f6a7b", "point": "energy"}],
                            "tz": "Asia/Taipei",
                            "period": "this_week",
                            "interval": "1d"
                        }
                    }
                }
            ],
            "computations": [
                {"name": "week_total", "title": "This week", "formula": "sum(usage.usage)", "unit": "kWh"},
                {"name": "first_day", "title": "First day", "formula": "daily.kwh[0]"}
            ],
            "constants": [
                {"name": "factor", "title": "Emission factor", "value": 0.494, "unit": "kgCO2e/kWh", "note": "The grid's published factor."},
                {"name": "line", "title": "Line", "value": "Line 2"}
            ]
        })
    }

    fn spec() -> Spec {
        serde_json::from_value(spec_json()).expect("spec")
    }

    fn problems(spec: &Spec) -> Vec<(String, SpecProblem)> {
        spec.validate()
            .into_iter()
            .map(|error| (error.path, error.problem))
            .collect()
    }

    #[test]
    fn a_good_spec_passes_and_round_trips() {
        let spec = spec();
        assert_eq!(problems(&spec), vec![]);
        assert_eq!(serde_json::to_value(&spec).expect("serialize"), spec_json());
        assert_eq!(
            validate_version("Energy", None, &spec, Some("<div></div>")),
            vec![]
        );
    }

    #[test]
    fn unknown_fields_are_refused_everywhere() {
        let paths: [&[&str]; 7] = [
            &[],
            &["datasets", "0"],
            &["datasets", "0", "source"],
            &["datasets", "1", "source", "request"],
            &["datasets", "1", "source", "request", "series", "0"],
            &["computations", "0"],
            &["constants", "0"],
        ];
        for path in paths {
            let mut value = spec_json();
            let mut target = &mut value;
            for key in path {
                target = match key.parse::<usize>() {
                    Ok(index) => &mut target[index],
                    Err(_) => &mut target[*key],
                };
            }
            target["page"] = json!("<script></script>");
            assert!(
                serde_json::from_value::<Spec>(value).is_err(),
                "an extra field at {path:?} was accepted"
            );
        }
    }

    #[test]
    fn sources_are_closed() {
        let mut value = spec_json();
        value["datasets"][0]["source"] = json!({"kind": "http", "url": "https://example.com"});
        assert!(serde_json::from_value::<Spec>(value).is_err());
        let mut value = spec_json();
        value["datasets"][0]["source"] = json!({"sql": "SELECT 1"});
        assert!(serde_json::from_value::<Spec>(value).is_err());
    }

    #[test]
    fn limits_are_enforced() {
        let mut spec = spec();
        let template = spec.datasets[0].clone();
        spec.datasets = (0..=MAX_DATASETS)
            .map(|index| Dataset {
                name: format!("d{index}"),
                ..template.clone()
            })
            .collect();
        spec.computations.clear();
        assert_eq!(
            problems(&spec),
            vec![(
                "/spec/datasets".to_owned(),
                SpecProblem::TooManyDatasets { max: MAX_DATASETS }
            )]
        );

        let mut spec = self::spec();
        spec.datasets.clear();
        spec.computations.clear();
        assert_eq!(
            problems(&spec),
            vec![("/spec/datasets".to_owned(), SpecProblem::NoDatasets)]
        );

        let mut spec = self::spec();
        let computation = spec.computations[0].clone();
        spec.computations = (0..=MAX_COMPUTATIONS)
            .map(|index| Computation {
                name: format!("c{index}"),
                ..computation.clone()
            })
            .collect();
        let constant = spec.constants[0].clone();
        spec.constants = (0..=MAX_CONSTANTS)
            .map(|index| Constant {
                name: format!("k{index}"),
                ..constant.clone()
            })
            .collect();
        assert_eq!(
            problems(&spec),
            vec![
                (
                    "/spec/computations".to_owned(),
                    SpecProblem::TooManyComputations {
                        max: MAX_COMPUTATIONS
                    }
                ),
                (
                    "/spec/constants".to_owned(),
                    SpecProblem::TooManyConstants { max: MAX_CONSTANTS }
                ),
            ]
        );

        let mut spec = self::spec();
        spec.datasets[0].max_rows = MAX_DATASET_ROWS + 1;
        spec.datasets[1].max_rows = 0;
        assert_eq!(
            problems(&spec),
            vec![
                (
                    "/spec/datasets/0/max_rows".to_owned(),
                    SpecProblem::MaxRowsOutOfRange { max: 2000 }
                ),
                (
                    "/spec/datasets/1/max_rows".to_owned(),
                    SpecProblem::MaxRowsOutOfRange { max: 2000 }
                ),
            ]
        );

        let mut spec = self::spec();
        spec.datasets[0].source = DatasetSource::Sql {
            sql: "S".repeat(16 * 1024 + 1),
        };
        assert_eq!(
            problems(&spec),
            vec![(
                "/spec/datasets/0/source/sql".to_owned(),
                SpecProblem::SqlTooLong { max_bytes: 16384 }
            )]
        );
        spec.datasets[0].source = DatasetSource::Sql {
            sql: "S".repeat(16 * 1024),
        };
        assert_eq!(problems(&spec), vec![]);
    }

    #[test]
    fn page_and_content_sizes_are_bounded() {
        let spec = spec();
        let page = "x".repeat(MAX_PAGE_BYTES);
        assert_eq!(validate_version("Energy", None, &spec, Some(&page)), vec![]);
        let page = "x".repeat(MAX_PAGE_BYTES + 1);
        let errors = validate_version("Energy", None, &spec, Some(&page));
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(errors[0].path, "/page");
        assert_eq!(
            validate_version("Energy", None, &spec, Some(" \n"))[0].problem,
            SpecProblem::PageEmpty
        );

        // A spec large enough that it and a page within the page limit exceed the content limit.
        let mut spec = spec;
        for constant in 0..MAX_CONSTANTS - 2 {
            spec.constants.push(Constant {
                name: format!("k{constant}"),
                title: "t".repeat(MAX_TITLE_CHARS),
                value: ConstantValue::Text("v".repeat(MAX_TEXT_VALUE_CHARS)),
                unit: None,
                note: Some("n".repeat(MAX_NOTE_CHARS)),
            });
        }
        assert_eq!(spec.validate(), vec![]);
        let spec_bytes = serde_json::to_vec(&spec).expect("json").len();
        let page = "x".repeat(MAX_CONTENT_BYTES - spec_bytes);
        assert!(page.len() <= MAX_PAGE_BYTES);
        assert_eq!(validate_version("Energy", None, &spec, Some(&page)), vec![]);
        let page = format!("{page}x");
        assert_eq!(
            validate_version("Energy", None, &spec, Some(&page))
                .into_iter()
                .map(|error| error.problem)
                .collect::<Vec<_>>(),
            vec![SpecProblem::ContentTooLarge {
                max_bytes: MAX_CONTENT_BYTES
            }]
        );
    }

    #[test]
    fn names_are_checked_and_shared() {
        let mut spec = spec();
        spec.datasets[0].name = "Daily".to_owned();
        spec.constants[0].name = "usage".to_owned();
        spec.computations[1].name = "line".to_owned();
        let found = problems(&spec);
        assert!(found.contains(&("/spec/datasets/0/name".to_owned(), SpecProblem::NameInvalid)));
        assert!(found.contains(&(
            "/spec/constants/0/name".to_owned(),
            SpecProblem::NameDuplicate
        )));
        assert!(found.contains(&(
            "/spec/constants/1/name".to_owned(),
            SpecProblem::NameDuplicate
        )));
    }

    #[test]
    fn texts_refuse_blank_long_and_hidden_characters() {
        for (title, ok) in [
            ("Energy", true),
            ("能源使用", true),
            ("", false),
            ("   ", false),
            ("Energy\nUse", false),
            ("Energy\u{202E}", false),
            ("Ener\u{200B}gy", false),
            (&"t".repeat(MAX_TITLE_CHARS), true),
            (&"t".repeat(MAX_TITLE_CHARS + 1), false),
            (&"能".repeat(MAX_TITLE_CHARS), true),
        ] {
            let mut spec = spec();
            spec.datasets[0].title = title.to_owned();
            assert_eq!(problems(&spec).is_empty(), ok, "{title:?}");
        }
        let mut spec = spec();
        spec.constants[1].value = ConstantValue::Text(String::new());
        spec.constants[0].unit = Some("u".repeat(MAX_UNIT_CHARS + 1));
        spec.constants[0].note = Some("\u{0007}".to_owned());
        spec.computations[0].unit = Some(String::new());
        spec.datasets[1].description = String::new();
        spec.constants[0].value = ConstantValue::Number(f64::NAN);
        let paths: Vec<String> = problems(&spec).into_iter().map(|(path, _)| path).collect();
        assert_eq!(
            paths,
            vec![
                "/spec/datasets/1/description",
                "/spec/computations/0/unit",
                "/spec/constants/0/value",
                "/spec/constants/0/unit",
                "/spec/constants/0/note",
                "/spec/constants/1/value",
            ]
        );
    }

    #[test]
    fn semantic_requests_are_checked() {
        let with_request = |change: &dyn Fn(&mut SemanticRequest)| {
            let mut spec = spec();
            if let DatasetSource::TelemetryUsage { request } = &mut spec.datasets[1].source {
                change(request);
            }
            problems(&spec)
                .into_iter()
                .map(|(path, problem)| {
                    (path.replace("/spec/datasets/1/source/request", ""), problem)
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(with_request(&|_| {}), vec![]);
        assert_eq!(
            with_request(&|request| request.series.clear()),
            vec![("/series".to_owned(), SpecProblem::SeriesCount { max: 20 })]
        );
        assert_eq!(
            with_request(&|request| request.series.push(request.series[0].clone())),
            vec![("/series/1".to_owned(), SpecProblem::SeriesDuplicate)]
        );
        assert_eq!(
            with_request(&|request| request.series[0].point = "-energy".to_owned()),
            vec![("/series/0/point".to_owned(), SpecProblem::PointInvalid)]
        );
        for tz in [
            "",
            "Asia/",
            "/Taipei",
            "Asia Taipei",
            "Asia/Taipei;",
            &"A".repeat(65),
        ] {
            assert_eq!(
                with_request(&|request| request.tz = tz.to_owned()),
                vec![("/tz".to_owned(), SpecProblem::TimeZoneInvalid)],
                "{tz:?}"
            );
        }
        for tz in [
            "UTC",
            "Etc/GMT+8",
            "America/Argentina/Buenos_Aires",
            "America/Port-au-Prince",
        ] {
            assert_eq!(
                with_request(&|request| request.tz = tz.to_owned()),
                vec![],
                "{tz}"
            );
        }
        for period in ["last_1_days", "last_366_days", "last_72_hours", "last_year"] {
            assert_eq!(
                with_request(&|request| request.period = Some(period.to_owned())),
                vec![],
                "{period}"
            );
        }
        for period in [
            "",
            "last_0_days",
            "last_367_days",
            "last_73_hours",
            "last_07_days",
            "next_week",
            "last_7_weeks",
        ] {
            assert_eq!(
                with_request(&|request| request.period = Some(period.to_owned())),
                vec![("/period".to_owned(), SpecProblem::PeriodInvalid)],
                "{period:?}"
            );
        }
        let time = |text: &str| DateTime::parse_from_rfc3339(text).expect("time");
        assert_eq!(
            with_request(&|request| {
                request.period = None;
                request.from = Some(time("2026-10-01T00:00:00+08:00"));
                request.to = Some(time("2026-10-08T00:00:00+08:00"));
            }),
            vec![]
        );
        for (period, from, to) in [
            (None, None, None),
            (Some("today"), Some("2026-10-01T00:00:00+08:00"), None),
            (None, Some("2026-10-01T00:00:00+08:00"), None),
            (
                None,
                Some("2026-10-08T00:00:00+08:00"),
                Some("2026-10-01T00:00:00+08:00"),
            ),
            (
                None,
                Some("2026-10-01T00:00:00+08:00"),
                Some("2026-09-30T16:00:00Z"),
            ),
        ] {
            assert_eq!(
                with_request(&|request| {
                    request.period = period.map(str::to_owned);
                    request.from = from.map(time);
                    request.to = to.map(time);
                }),
                vec![(String::new(), SpecProblem::RangeInvalid)],
                "{period:?} {from:?} {to:?}"
            );
        }
    }

    #[test]
    fn semantic_request_times_keep_their_offset() {
        let request: SemanticRequest = serde_json::from_value(json!({
            "series": [{"device_id": "0190a5b2-7c3d-7e4f-8a1b-2c3d4e5f6a7b", "point": "energy"}],
            "tz": "Asia/Taipei",
            "from": "2026-10-01T00:00:00+08:00",
            "to": "2026-10-08T00:00:00+08:00"
        }))
        .expect("request");
        let value = serde_json::to_value(&request).expect("serialize");
        assert_eq!(value["from"], "2026-10-01T00:00:00+08:00");
        assert_eq!(value["interval"], "total");
    }

    #[test]
    fn formulas_are_checked_against_the_datasets() {
        let mut spec = spec();
        spec.computations[0].formula = "sum(usage.kwh)".to_owned();
        spec.computations[1].formula = "sum(nope.kwh) +".to_owned();
        assert_eq!(
            problems(&spec)
                .into_iter()
                .map(|(path, problem)| (path, problem.formula_class()))
                .collect::<Vec<_>>(),
            vec![
                (
                    "/spec/computations/0/formula".to_owned(),
                    Some(FormulaErrorClass::UnknownColumn)
                ),
                (
                    "/spec/computations/1/formula".to_owned(),
                    Some(FormulaErrorClass::UnexpectedEnd)
                ),
            ]
        );

        // A SQL dataset's columns are known after a trial run.
        let spec = self::spec();
        let result: DatasetResult = serde_json::from_value(json!({
            "dataset": "daily",
            "columns": [{"name": "day", "type": "time"}],
            "rows": [],
            "row_count": 0,
            "meta": {
                "queried_at": "2026-10-26T01:02:03Z",
                "duration_ms": 5,
                "source": {"kind": "sql", "queryable_from": {
                    "tenant_telemetry": "2026-01-01T00:00:00Z",
                    "tenant_telemetry_1m": "2026-01-01T00:00:00Z",
                    "tenant_telemetry_1h": "2026-01-01T00:00:00Z"
                }}
            }
        }))
        .expect("result");
        assert_eq!(
            spec.check_formulas(std::slice::from_ref(&result))
                .into_iter()
                .map(|error| (error.path, error.problem.formula_class()))
                .collect::<Vec<_>>(),
            vec![(
                "/spec/computations/1/formula".to_owned(),
                Some(FormulaErrorClass::UnknownColumn)
            )]
        );
    }

    #[test]
    fn the_public_part_has_no_query() {
        let public = serde_json::to_value(spec().public()).expect("serialize");
        let text = public.to_string();
        assert!(!text.contains("SELECT"));
        assert!(!text.contains("Asia/Taipei"));
        assert_eq!(public["datasets"][1]["source"], "telemetry_usage");
        assert_eq!(public["computations"][0]["formula"], "sum(usage.usage)");
        assert_eq!(public["constants"][1]["value"], "Line 2");
    }

    #[test]
    fn every_problem_has_a_message() {
        let error = SpecError::new("/spec", SpecProblem::SqlEmpty);
        assert_eq!(
            serde_json::to_value(&error).expect("serialize"),
            json!({"path": "/spec", "problem": "sql_empty"})
        );
        assert_eq!(error.message(), "The SQL is empty.");
        let formula = SpecError::new(
            "/spec/computations/0/formula",
            SpecProblem::Formula(Formula::parse("1 +").expect_err("incomplete")),
        );
        assert_eq!(
            serde_json::to_value(&formula).expect("serialize"),
            json!({"path": "/spec/computations/0/formula", "problem": "formula", "class": "unexpected_end", "position": 4})
        );
    }

    /// One of every problem. The match has no wildcard, so a new problem does not compile until
    /// it is added here too.
    fn every_problem() -> Vec<SpecProblem> {
        use SpecProblem as P;
        let all = vec![
            P::NoDatasets,
            P::TooManyDatasets { max: 8 },
            P::TooManyComputations { max: 32 },
            P::TooManyConstants { max: 32 },
            P::NameInvalid,
            P::NameDuplicate,
            P::TitleInvalid { max_chars: 120 },
            P::DescriptionInvalid { max_chars: 500 },
            P::UnitInvalid { max_chars: 24 },
            P::NoteInvalid { max_chars: 300 },
            P::TextValueInvalid { max_chars: 200 },
            P::NumberNotFinite,
            P::MaxRowsOutOfRange { max: 2000 },
            P::SqlEmpty,
            P::SqlTooLong { max_bytes: 16384 },
            P::SeriesCount { max: 20 },
            P::SeriesDuplicate,
            P::PointInvalid,
            P::TimeZoneInvalid,
            P::RangeInvalid,
            P::PeriodInvalid,
            P::Formula(Formula::parse("1 +").expect_err("incomplete")),
            P::PageEmpty,
            P::PageTooLarge { max_bytes: 1 },
            P::ContentTooLarge { max_bytes: 1 },
            P::NoComponents,
            P::TooManyComponents { max: 12 },
            P::CustomPageDuplicate,
            P::CustomPageWithoutPage,
            P::PageWithoutCustomPage,
            P::CustomPageDisabled,
            P::ComponentInvalid,
            P::ComponentKindUnknown,
            P::ComponentFieldUnknown,
            P::ComponentFieldMissing,
            P::ComponentFieldInvalid,
            P::DatasetUnknown,
            P::ComputationUnknown,
            P::ConstantUnknown,
            P::ColumnInvalid,
            P::ColumnDuplicate,
            P::YColumnsCount { max: 6 },
            P::TableColumnsCount { max: 16 },
            P::ThresholdsCount { max: 4 },
            P::ThresholdNotNumber,
            P::ThresholdsNotAscending,
            P::RowOutOfRange,
            P::RowMissing,
            P::ColumnUnknown,
            P::ColumnNotTime,
            P::ColumnNotNumber,
            P::ColumnNotText,
            P::ColumnNotCategory,
            P::XDuplicate,
            P::SeriesTooMany { max: 6 },
        ];
        for problem in &all {
            match problem {
                P::NoDatasets
                | P::TooManyDatasets { .. }
                | P::TooManyComputations { .. }
                | P::TooManyConstants { .. }
                | P::NameInvalid
                | P::NameDuplicate
                | P::TitleInvalid { .. }
                | P::DescriptionInvalid { .. }
                | P::UnitInvalid { .. }
                | P::NoteInvalid { .. }
                | P::TextValueInvalid { .. }
                | P::NumberNotFinite
                | P::MaxRowsOutOfRange { .. }
                | P::SqlEmpty
                | P::SqlTooLong { .. }
                | P::SeriesCount { .. }
                | P::SeriesDuplicate
                | P::PointInvalid
                | P::TimeZoneInvalid
                | P::RangeInvalid
                | P::PeriodInvalid
                | P::Formula(_)
                | P::PageEmpty
                | P::PageTooLarge { .. }
                | P::ContentTooLarge { .. }
                | P::NoComponents
                | P::TooManyComponents { .. }
                | P::CustomPageDuplicate
                | P::CustomPageWithoutPage
                | P::PageWithoutCustomPage
                | P::CustomPageDisabled
                | P::ComponentInvalid
                | P::ComponentKindUnknown
                | P::ComponentFieldUnknown
                | P::ComponentFieldMissing
                | P::ComponentFieldInvalid
                | P::DatasetUnknown
                | P::ComputationUnknown
                | P::ConstantUnknown
                | P::ColumnInvalid
                | P::ColumnDuplicate
                | P::YColumnsCount { .. }
                | P::TableColumnsCount { .. }
                | P::ThresholdsCount { .. }
                | P::ThresholdNotNumber
                | P::ThresholdsNotAscending
                | P::RowOutOfRange
                | P::RowMissing
                | P::ColumnUnknown
                | P::ColumnNotTime
                | P::ColumnNotNumber
                | P::ColumnNotText
                | P::ColumnNotCategory
                | P::XDuplicate
                | P::SeriesTooMany { .. } => {}
            }
        }
        all
    }

    #[test]
    fn every_problem_has_a_key_named_after_it() {
        let mut keys = BTreeSet::new();
        for problem in every_problem() {
            let value = serde_json::to_value(problem).expect("serialize");
            let name = value["problem"].as_str().expect("problem name");
            assert_eq!(
                problem.message_key().as_str(),
                format!("dashboard.spec.{name}"),
                "{problem:?}"
            );
            assert!(keys.insert(name.to_owned()), "{name} twice");
            assert!(!problem.message().is_empty(), "{problem:?}");
            // The details the key's translations may use are the problem's own numbers.
            for (field, detail) in value.as_object().expect("object") {
                assert!(
                    matches!(
                        field.as_str(),
                        "problem" | "max" | "max_chars" | "max_bytes" | "class" | "position"
                    ),
                    "{field} of {name}"
                );
                assert!(field == "problem" || field == "class" || detail.is_u64());
            }
        }
    }
}
