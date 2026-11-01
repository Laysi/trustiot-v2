//! The spec of a dashboard version: its datasets, computations and constants, and the checks
//! that need no I/O.
//!
//! The `dashboard` tool writes a spec (design record 「資料模型」); the assistant validates it
//! with [`validate_version`] before it runs anything, stores it with the version, and gives the
//! page its public part ([`PublicSpec`]). Every type here refuses fields it does not know: the
//! model writes them.
//!
//! Datasets, computations and constants share one namespace of [`is_name`] names, so a
//! reference or a formula names each without saying which kind it is.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, FixedOffset};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::formula::{DatasetShape, Formula, FormulaError, FormulaErrorClass};
use super::names::{NAME_PATTERN, is_name};
use super::result::{DatasetResult, MAX_DATASET_ROWS, semantic_columns};
use crate::DeviceId;
use crate::api::telemetry_query::MAX_SQL_BYTES;

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

/// Longest dashboard, dataset, computation or constant title, in characters.
pub const MAX_TITLE_CHARS: usize = 120;

/// Longest description of a dashboard or a dataset, in characters: a sentence or two on what
/// the data is and how it is computed.
pub const MAX_DESCRIPTION_CHARS: usize = 500;

/// Longest unit, in characters (`kWh`, `°C`, `NT$/kWh`).
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

/// A version's datasets, computations and constants.
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

/// The part of a spec the page and the check see: the datasets without their queries, and
/// every computation and constant. Internal callers accept fields they do not know.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PublicSpec {
    pub datasets: Vec<PublicDataset>,
    pub computations: Vec<Computation>,
    pub constants: Vec<Constant>,
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
    fn new(path: impl Into<String>, problem: SpecProblem) -> Self {
        Self {
            path: path.into(),
            problem,
        }
    }
}

/// The closed set of [`SpecError`] problems.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "problem", rename_all = "snake_case")]
pub enum SpecProblem {
    NoDatasets,
    TooManyDatasets { max: usize },
    TooManyComputations { max: usize },
    TooManyConstants { max: usize },
    NameInvalid,
    NameDuplicate,
    TitleInvalid { max_chars: usize },
    DescriptionInvalid { max_chars: usize },
    UnitInvalid { max_chars: usize },
    NoteInvalid { max_chars: usize },
    TextValueInvalid { max_chars: usize },
    NumberNotFinite,
    MaxRowsOutOfRange { max: u32 },
    SqlEmpty,
    SqlTooLong { max_bytes: u32 },
    SeriesCount { max: usize },
    SeriesDuplicate,
    PointInvalid,
    TimeZoneInvalid,
    RangeInvalid,
    PeriodInvalid,
    Formula(FormulaError),
    PageEmpty,
    PageTooLarge { max_bytes: usize },
    ContentTooLarge { max_bytes: usize },
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
        }
    }
}

impl Spec {
    /// Checks everything that needs no I/O: the counts, every name, title, description, unit,
    /// note and value, every source, and every formula against the declared datasets (and, for
    /// semantic datasets, their fixed columns). Paths start at `/spec`.
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
        }
    }

    /// The dataset named `name`.
    #[must_use]
    pub fn dataset(&self, name: &str) -> Option<&Dataset> {
        self.datasets.iter().find(|dataset| dataset.name == name)
    }
}

/// Checks a whole version: the dashboard's title and optional description, the spec
/// ([`Spec::validate`]), the page (not empty, at most [`MAX_PAGE_BYTES`]) and the content's size
/// (at most [`MAX_CONTENT_BYTES`]).
#[must_use]
pub fn validate_version(
    title: &str,
    description: Option<&str>,
    spec: &Spec,
    page: &str,
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
    let spec_bytes = serde_json::to_vec(spec).map_or(usize::MAX, |json| json.len());
    if spec_bytes.saturating_add(page.len()) > MAX_CONTENT_BYTES {
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
struct TextRule {
    max_chars: usize,
    problem: fn(usize) -> SpecProblem,
}

impl TextRule {
    const TITLE: Self = Self {
        max_chars: MAX_TITLE_CHARS,
        problem: |max_chars| SpecProblem::TitleInvalid { max_chars },
    };
    const DESCRIPTION: Self = Self {
        max_chars: MAX_DESCRIPTION_CHARS,
        problem: |max_chars| SpecProblem::DescriptionInvalid { max_chars },
    };
    const UNIT: Self = Self {
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
fn text(errors: &mut Vec<SpecError>, at: &str, field: &str, value: &str, rule: TextRule) {
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
            validate_version("Energy", None, &spec, "<div></div>"),
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
        assert_eq!(validate_version("Energy", None, &spec, &page), vec![]);
        let page = "x".repeat(MAX_PAGE_BYTES + 1);
        let errors = validate_version("Energy", None, &spec, &page);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(errors[0].path, "/page");
        assert_eq!(
            validate_version("Energy", None, &spec, " \n")[0].problem,
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
        assert_eq!(validate_version("Energy", None, &spec, &page), vec![]);
        let page = format!("{page}x");
        assert_eq!(
            validate_version("Energy", None, &spec, &page)
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
}
