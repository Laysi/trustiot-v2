//! The component list of a dashboard version (decision 33 as revised on 2026-11-01,
//! Laysi/trustiot-v2#927; design record `docs/architecture/ai-assistant.md`, 「元件模式」 and
//! 「兩條路的正確性」).
//!
//! A version shows a list of components in order. Each item is a platform component (a time
//! series, a bar chart, a KPI figure with an optional trend, a table, a status) that the SPA
//! draws from the spec and the dataset results, or the one custom page (HTML and script the model
//! wrote, run in the sandbox). The model only picks components and fills their fields: every
//! field is a name (a dataset, a column, a computation, a constant), a row index, or text (a
//! title, a unit). No field takes a colour, a style, HTML, a URL or code, and every type refuses
//! fields it does not know.
//!
//! The checks come in three steps, all without I/O:
//!
//! 1. [`Component::parse`] reads one component the model wrote and names what is wrong with it
//!    in closed classes: an unknown kind, an unknown field, a missing field, a field of the wrong
//!    form. A spec read from storage is parsed with serde, which refuses the same things without
//!    saying which.
//! 2. `Spec::validate` and `validate_version` ([`super::spec`]) check the list: its length, the
//!    titles and units, that every dataset, computation and constant named exists, that
//!    thresholds are number constants from low to high, and that the custom page item and the
//!    page agree.
//! 3. [`Spec::check_component_columns`] and [`Spec::check_components`] check what only a result
//!    tells: that every column named exists and its type suits the component (a time series' x
//!    is a time, its y are numbers), and that no series has two rows at one x. The assistant
//!    calls them after it ran the datasets of a write.
//!
//! # Versions written before components
//!
//! A version written before 2026-11-01 has no component list, only a page. It reads as a list
//! of one item, the custom page ([`Spec::layout`]), and it stays valid: `validate_version`
//! accepts an empty list when there is a page.

use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::BTreeSet;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::ToSchema;

use super::names::{COLUMN_NAME_PATTERN, NAME_PATTERN, is_column_name};
use super::result::{ColumnKind, DatasetResult, ResultColumn};
use super::spec::{
    Constant, ConstantValue, Dataset, Spec, SpecError, SpecProblem, TextRule, text as check_text,
};

/// The most items in a version's component list, the custom page included. A dashboard is a
/// screen or two of panels, and every component draws at every view; with at most
/// `spec::MAX_DATASETS` datasets, twelve leaves room for a chart and a figure or two per
/// dataset while keeping the spec short enough for the model to read back whole.
pub const MAX_COMPONENTS: usize = 12;

/// The most series one time series or bar chart draws: its `y` columns, and with a `series`
/// column, its `y` columns times the distinct values of that column in the result. The SPA's
/// chart components have six series colors per theme, each checked for WCAG 2.2 AA contrast and
/// each adjacent pair for color-vision deficiency; a seventh series would have to reuse a color
/// and could no longer be told apart, so the components show an error instead. Checked here so
/// the model learns it at write time: [`Spec::validate`] bounds the `y` columns, and
/// [`Spec::check_components`] the series a result splits into.
pub const MAX_SERIES: usize = 6;

/// The most columns of one table: as wide as a screen can show readably (a result has at most
/// `result::MAX_COLUMNS`).
pub const MAX_TABLE_COLUMNS: usize = 16;

/// The most thresholds of one component. Two thresholds already make three bands (normal,
/// warning, alarm); four make five bands on a status, or four reference lines on a chart.
pub const MAX_THRESHOLDS: usize = 4;

/// One item of a version's component list, tagged by `kind`.
///
/// `title` and `unit` are shown as text, never as markup. `unit` is optional everywhere: a
/// count, a ratio or a power factor has none, and a made-up unit would be wrong on the screen.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Component {
    /// A trend over time, one or more lines: `x` is a time column, every `y` a number column.
    TimeSeries(SeriesChart),
    /// A comparison between categories or periods: `x` is a text or time column, every `y` a
    /// number column.
    Bar(SeriesChart),
    /// One value, with an optional trend and an optional value to compare it with.
    Kpi(Kpi),
    /// The rows of a dataset, column by column.
    Table(Table),
    /// Which band between thresholds one value falls in.
    Status(Status),
    /// The custom page's place in the list: the page itself is the version's `page`. At most
    /// one per version, and only with a page.
    CustomPage(CustomPage),
}

/// The OpenAPI schema of [`Component`], built from the derived schemas of its items. The
/// derived one composes each kind with `allOf` from the item's struct, whose
/// `deny_unknown_fields` makes it `additionalProperties: false`; by JSON Schema that refuses the
/// `kind` field itself, so no valid component would validate. Here each kind is its item's own
/// closed object with `kind` added to the properties and the required fields, which is what
/// serde reads.
impl utoipa::PartialSchema for Component {
    fn schema() -> utoipa::openapi::RefOr<utoipa::openapi::schema::Schema> {
        use utoipa::openapi::schema::OneOfBuilder;

        let mut kinds = OneOfBuilder::new().description(Some(
            "One item of a version's component list, tagged by `kind`.\n\n`title` and `unit` are \
             shown as text, never as markup. `unit` is optional everywhere: a count, a ratio or \
             a power factor has none, and a made-up unit would be wrong on the screen.",
        ));
        for kind in ComponentKind::ALL {
            let (item, description) = match kind {
                ComponentKind::TimeSeries => (
                    SeriesChart::schema(),
                    "A trend over time, one or more lines: `x` is a time column, every `y` a \
                     number column.",
                ),
                ComponentKind::Bar => (
                    SeriesChart::schema(),
                    "A comparison between categories or periods: `x` is a text or time column, \
                     every `y` a number column.",
                ),
                ComponentKind::Kpi => (
                    Kpi::schema(),
                    "One value, with an optional trend and an optional value to compare it with.",
                ),
                ComponentKind::Table => {
                    (Table::schema(), "The rows of a dataset, column by column.")
                }
                ComponentKind::Status => (
                    Status::schema(),
                    "Which band between thresholds one value falls in.",
                ),
                ComponentKind::CustomPage => (
                    CustomPage::schema(),
                    "The custom page's place in the list: the page itself is the version's \
                     `page`. At most one per version, and only with a page.",
                ),
            };
            kinds = kinds.item(tagged_schema(item, kind, description));
        }
        kinds.into()
    }
}

impl ToSchema for Component {
    fn name() -> Cow<'static, str> {
        Cow::Borrowed("DashboardComponent")
    }

    fn schemas(
        schemas: &mut Vec<(
            String,
            utoipa::openapi::RefOr<utoipa::openapi::schema::Schema>,
        )>,
    ) {
        // The items are inlined; what they refer to is registered by their own derives.
        <SeriesChart as ToSchema>::schemas(schemas);
        <Kpi as ToSchema>::schemas(schemas);
        <Table as ToSchema>::schemas(schemas);
        <Status as ToSchema>::schemas(schemas);
        <CustomPage as ToSchema>::schemas(schemas);
    }
}

/// An item's derived object schema with the `kind` property added and required. Every item is
/// a struct, so its derived schema is an object; anything else is returned as it came, and the
/// test `the_schema_closes_each_kind_with_its_tag` fails.
fn tagged_schema(
    item: utoipa::openapi::RefOr<utoipa::openapi::schema::Schema>,
    kind: ComponentKind,
    description: &str,
) -> utoipa::openapi::RefOr<utoipa::openapi::schema::Schema> {
    use utoipa::openapi::RefOr;
    use utoipa::openapi::schema::{ObjectBuilder, Schema, Type};

    let RefOr::T(Schema::Object(mut object)) = item else {
        return item;
    };
    object.properties.insert(
        "kind".to_owned(),
        ObjectBuilder::new()
            .schema_type(Type::String)
            .enum_values(Some([kind.as_str()]))
            .into(),
    );
    object.required.push("kind".to_owned());
    object.title = Some(kind.as_str().to_owned());
    object.description = Some(description.to_owned());
    RefOr::T(Schema::Object(object))
}

impl Component {
    /// The component's kind.
    #[must_use]
    pub const fn kind(&self) -> ComponentKind {
        match self {
            Self::TimeSeries(_) => ComponentKind::TimeSeries,
            Self::Bar(_) => ComponentKind::Bar,
            Self::Kpi(_) => ComponentKind::Kpi,
            Self::Table(_) => ComponentKind::Table,
            Self::Status(_) => ComponentKind::Status,
            Self::CustomPage(_) => ComponentKind::CustomPage,
        }
    }

    /// Reads one component the model wrote, at the JSON pointer `at` of the tool's input
    /// (`/spec/components/3`), naming every problem in a closed class: the item is not an
    /// object ([`SpecProblem::ComponentInvalid`]), its `kind` is missing or unknown
    /// ([`SpecProblem::ComponentKindUnknown`]), a field is unknown, missing or of the wrong
    /// form ([`SpecProblem::ComponentFieldUnknown`], [`SpecProblem::ComponentFieldMissing`],
    /// [`SpecProblem::ComponentFieldInvalid`]). The path of an unknown field names it only when
    /// it is a plain lowercase name; otherwise it is `at`, so the model's text never reaches a
    /// path.
    ///
    /// What parses is not yet valid: `validate_version` checks the rest.
    ///
    /// # Errors
    ///
    /// Every problem found, at least one.
    pub fn parse(value: &Value, at: &str) -> Result<Self, Vec<SpecError>> {
        let Some(object) = value.as_object() else {
            return Err(vec![SpecError::new(at, SpecProblem::ComponentInvalid)]);
        };
        let Some(kind) = object
            .get("kind")
            .and_then(Value::as_str)
            .and_then(ComponentKind::parse)
        else {
            return Err(vec![SpecError::new(
                format!("{at}/kind"),
                SpecProblem::ComponentKindUnknown,
            )]);
        };
        let fields = kind.fields();
        let mut errors = Vec::new();
        for (key, value) in object {
            if key == "kind" {
                continue;
            }
            match fields.iter().find(|field| field.name == key) {
                None => {
                    let path = if is_plain_key(key) {
                        format!("{at}/{key}")
                    } else {
                        at.to_owned()
                    };
                    errors.push(SpecError::new(path, SpecProblem::ComponentFieldUnknown));
                }
                Some(field) if !(field.valid)(value) => errors.push(SpecError::new(
                    format!("{at}/{key}"),
                    SpecProblem::ComponentFieldInvalid,
                )),
                Some(_) => {}
            }
        }
        for field in fields
            .iter()
            .filter(|field| field.required && !object.contains_key(field.name))
        {
            errors.push(SpecError::new(
                format!("{at}/{}", field.name),
                SpecProblem::ComponentFieldMissing,
            ));
        }
        if !errors.is_empty() {
            return Err(errors);
        }
        // The field table and the types agree (tests), so this only fails if they drift.
        Self::deserialize(value)
            .map_err(|_| vec![SpecError::new(at, SpecProblem::ComponentInvalid)])
    }
}

/// The kinds of [`Component`], as `kind` spells them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComponentKind {
    TimeSeries,
    Bar,
    Kpi,
    Table,
    Status,
    CustomPage,
}

impl ComponentKind {
    /// Every kind.
    pub const ALL: [Self; 6] = [
        Self::TimeSeries,
        Self::Bar,
        Self::Kpi,
        Self::Table,
        Self::Status,
        Self::CustomPage,
    ];

    /// The `kind` value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TimeSeries => "time_series",
            Self::Bar => "bar",
            Self::Kpi => "kpi",
            Self::Table => "table",
            Self::Status => "status",
            Self::CustomPage => "custom_page",
        }
    }

    /// The kind spelled `kind`.
    #[must_use]
    pub fn parse(kind: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|known| known.as_str() == kind)
    }

    /// The fields of this kind besides `kind`, as [`Component::parse`] checks them.
    fn fields(self) -> &'static [Field] {
        match self {
            Self::TimeSeries | Self::Bar => SERIES_CHART_FIELDS,
            Self::Kpi => KPI_FIELDS,
            Self::Table => TABLE_FIELDS,
            Self::Status => STATUS_FIELDS,
            Self::CustomPage => &[],
        }
    }

    /// The names of this kind's required and optional fields, besides `kind`.
    #[must_use]
    pub fn field_names(self) -> (Vec<&'static str>, Vec<&'static str>) {
        let (required, optional): (Vec<&Field>, Vec<&Field>) =
            self.fields().iter().partition(|field| field.required);
        (
            required.iter().map(|field| field.name).collect(),
            optional.iter().map(|field| field.name).collect(),
        )
    }
}

/// One field of a kind: its name, whether it is required, and whether a JSON value is of its
/// type.
struct Field {
    name: &'static str,
    required: bool,
    valid: fn(&Value) -> bool,
}

impl Field {
    const fn required(name: &'static str, valid: fn(&Value) -> bool) -> Self {
        Self {
            name,
            required: true,
            valid,
        }
    }

    const fn optional(name: &'static str, valid: fn(&Value) -> bool) -> Self {
        Self {
            name,
            required: false,
            valid,
        }
    }
}

/// Whether `value` reads as a `T`.
fn reads_as<T: DeserializeOwned>(value: &Value) -> bool {
    T::deserialize(value).is_ok()
}

const SERIES_CHART_FIELDS: &[Field] = &[
    Field::required("title", reads_as::<String>),
    Field::required("dataset", reads_as::<String>),
    Field::required("x", reads_as::<String>),
    Field::required("y", reads_as::<Vec<String>>),
    Field::optional("series", reads_as::<Option<String>>),
    Field::optional("unit", reads_as::<Option<String>>),
    Field::optional("thresholds", reads_as::<Vec<String>>),
];

const KPI_FIELDS: &[Field] = &[
    Field::required("title", reads_as::<String>),
    Field::required("value", reads_as::<ValueSource>),
    Field::optional("trend", reads_as::<Option<Trend>>),
    Field::optional("compare", reads_as::<Option<String>>),
    Field::optional("unit", reads_as::<Option<String>>),
];

const TABLE_FIELDS: &[Field] = &[
    Field::required("title", reads_as::<String>),
    Field::required("dataset", reads_as::<String>),
    Field::required("columns", reads_as::<Vec<TableColumn>>),
];

const STATUS_FIELDS: &[Field] = &[
    Field::required("title", reads_as::<String>),
    Field::required("value", reads_as::<ValueSource>),
    Field::required("thresholds", reads_as::<Vec<String>>),
    Field::optional("unit", reads_as::<Option<String>>),
];

/// A key short and plain enough to put in a path: 1 to 32 lowercase letters, digits and `_`.
fn is_plain_key(key: &str) -> bool {
    (1..=32).contains(&key.len())
        && key
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

/// A time series or a bar chart: one point or bar per row and `y` column.
///
/// Without `series`, each `y` column is one line (or one bar per x). With `series`, the rows
/// split by that text column's value, one line (or bar) per value and `y` column. The component
/// never adds rows together: one series has at most one row at each x
/// ([`Spec::check_components`]).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
#[schema(as = DashboardSeriesChart)]
pub struct SeriesChart {
    /// 1 to `spec::MAX_TITLE_CHARS` characters.
    pub title: String,
    /// The dataset's name.
    #[schema(pattern = "^[a-z][a-z0-9_]{0,47}$")]
    pub dataset: String,
    /// The column along the x axis: a time column for a time series, a text or time column for
    /// a bar chart.
    #[schema(pattern = "^[A-Za-z_][A-Za-z0-9_]{0,63}$")]
    pub x: String,
    /// 1 to [`MAX_SERIES`] number columns.
    pub y: Vec<String>,
    /// A text column whose values split the rows into lines (or side-by-side bars); the `y`
    /// columns times its distinct values in the result are at most [`MAX_SERIES`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(pattern = "^[A-Za-z_][A-Za-z0-9_]{0,63}$")]
    pub series: Option<String>,
    /// 1 to `spec::MAX_UNIT_CHARS` characters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    /// Up to [`MAX_THRESHOLDS`] number constants, drawn as reference lines, from low to high.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub thresholds: Vec<String>,
}

/// Where a KPI's or a status' value comes from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ValueSource {
    /// One cell of a dataset: a number column, `row` the 0-based index in the result, below the
    /// dataset's `max_rows`.
    Cell {
        dataset: String,
        column: String,
        row: u32,
    },
    /// A computation's value.
    Computation { name: String },
}

/// The OpenAPI schema of [`ValueSource`], written out. The derived one leaves each variant's
/// object open, while serde refuses any field besides the variant's own and `kind`; here each
/// variant is one closed object, as for [`Component`].
impl utoipa::PartialSchema for ValueSource {
    fn schema() -> utoipa::openapi::RefOr<utoipa::openapi::schema::Schema> {
        use utoipa::openapi::schema::{
            AdditionalProperties, KnownFormat, ObjectBuilder, OneOfBuilder, Schema, SchemaFormat,
            Type,
        };

        let pattern = |pattern: &str| {
            ObjectBuilder::new()
                .schema_type(Type::String)
                .pattern(Some(pattern))
        };
        let kind = |kind: &str| {
            ObjectBuilder::new()
                .schema_type(Type::String)
                .enum_values(Some([kind]))
        };
        let cell = ObjectBuilder::new()
            .schema_type(Type::Object)
            .title(Some("cell"))
            .description(Some(
                "One cell of a dataset: a number column, `row` the 0-based index in the result, \
                 below the dataset's `max_rows`.",
            ))
            .property("kind", kind("cell"))
            .property("dataset", pattern(NAME_PATTERN))
            .property("column", pattern(COLUMN_NAME_PATTERN))
            .property(
                "row",
                ObjectBuilder::new()
                    .schema_type(Type::Integer)
                    .format(Some(SchemaFormat::KnownFormat(KnownFormat::Int32)))
                    .minimum(Some(0)),
            )
            .required("kind")
            .required("dataset")
            .required("column")
            .required("row")
            .additional_properties(Some(AdditionalProperties::<Schema>::FreeForm(false)));
        let computation = ObjectBuilder::new()
            .schema_type(Type::Object)
            .title(Some("computation"))
            .description(Some("A computation's value."))
            .property("kind", kind("computation"))
            .property("name", pattern(NAME_PATTERN))
            .required("kind")
            .required("name")
            .additional_properties(Some(AdditionalProperties::<Schema>::FreeForm(false)));
        OneOfBuilder::new()
            .description(Some("Where a KPI's or a status' value comes from."))
            .item(cell)
            .item(computation)
            .into()
    }
}

impl ToSchema for ValueSource {
    fn name() -> Cow<'static, str> {
        Cow::Borrowed("DashboardValueSource")
    }
}

/// A KPI's trend: a small line of a dataset's `y` over its time column `x`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
#[schema(as = DashboardKpiTrend)]
pub struct Trend {
    #[schema(pattern = "^[a-z][a-z0-9_]{0,47}$")]
    pub dataset: String,
    /// A time column.
    #[schema(pattern = "^[A-Za-z_][A-Za-z0-9_]{0,63}$")]
    pub x: String,
    /// A number column.
    #[schema(pattern = "^[A-Za-z_][A-Za-z0-9_]{0,63}$")]
    pub y: String,
}

/// One value shown large, with an optional trend and an optional value to compare it with.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
#[schema(as = DashboardKpi)]
pub struct Kpi {
    pub title: String,
    pub value: ValueSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trend: Option<Trend>,
    /// A computation shown next to the value for comparison (last week's, the target's share).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(pattern = "^[a-z][a-z0-9_]{0,47}$")]
    pub compare: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
}

/// One column of a table, in the order shown.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
#[schema(as = DashboardTableColumn)]
pub struct TableColumn {
    #[schema(pattern = "^[A-Za-z_][A-Za-z0-9_]{0,63}$")]
    pub column: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
}

/// A dataset's rows, with the columns listed, in order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
#[schema(as = DashboardTable)]
pub struct Table {
    pub title: String,
    #[schema(pattern = "^[a-z][a-z0-9_]{0,47}$")]
    pub dataset: String,
    /// 1 to [`MAX_TABLE_COLUMNS`] distinct columns.
    pub columns: Vec<TableColumn>,
}

/// Which band one value falls in: below the first threshold, between two, or at or above the
/// last.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
#[schema(as = DashboardStatus)]
pub struct Status {
    pub title: String,
    pub value: ValueSource,
    /// 1 to [`MAX_THRESHOLDS`] number constants, from low to high (strictly increasing).
    pub thresholds: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
}

/// The custom page's item: no fields.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
#[schema(as = DashboardCustomPage)]
pub struct CustomPage {}

impl Spec {
    /// The list the screen shows, in order: the component list, or, for a version without one
    /// (written before 2026-11-01) that has a page, that page as the only item. Empty only for
    /// a version `validate_version` refuses.
    #[must_use]
    pub fn layout(&self, has_page: bool) -> Cow<'_, [Component]> {
        if self.components.is_empty() && has_page {
            Cow::Owned(vec![Component::CustomPage(CustomPage {})])
        } else {
            Cow::Borrowed(&self.components)
        }
    }

    /// Checks every component's columns against the columns of its dataset's result, as
    /// `columns` gives them by dataset name: that each column exists and its type suits the
    /// component. A dataset `columns` has nothing for is reported as unknown. Paths start at
    /// `/spec`. Needs no I/O; the assistant calls it with a write's results, and the SPA does
    /// the same at every view (design record 「兩條路的正確性」).
    #[must_use]
    pub fn check_component_columns<'a>(
        &self,
        columns: impl Fn(&str) -> Option<&'a [ResultColumn]>,
    ) -> Vec<SpecError> {
        let mut errors = Vec::new();
        for (index, component) in self.components.iter().enumerate() {
            let at = format!("/spec/components/{index}");
            match component {
                Component::TimeSeries(chart) | Component::Bar(chart) => {
                    let Some(found) = result_columns(&mut errors, &at, &chart.dataset, &columns)
                    else {
                        continue;
                    };
                    let x = if matches!(component, Component::TimeSeries(_)) {
                        Expect::Time
                    } else {
                        Expect::Category
                    };
                    expect(&mut errors, format!("{at}/x"), found, &chart.x, x);
                    for (y_index, y) in chart.y.iter().enumerate() {
                        expect(
                            &mut errors,
                            format!("{at}/y/{y_index}"),
                            found,
                            y,
                            Expect::Number,
                        );
                    }
                    if let Some(series) = &chart.series {
                        expect(
                            &mut errors,
                            format!("{at}/series"),
                            found,
                            series,
                            Expect::Text,
                        );
                    }
                }
                Component::Kpi(kpi) => {
                    check_value(&mut errors, &at, &kpi.value, &columns);
                    if let Some(trend) = &kpi.trend {
                        let at = format!("{at}/trend");
                        if let Some(found) =
                            result_columns(&mut errors, &at, &trend.dataset, &columns)
                        {
                            expect(
                                &mut errors,
                                format!("{at}/x"),
                                found,
                                &trend.x,
                                Expect::Time,
                            );
                            expect(
                                &mut errors,
                                format!("{at}/y"),
                                found,
                                &trend.y,
                                Expect::Number,
                            );
                        }
                    }
                }
                Component::Table(table) => {
                    let Some(found) = result_columns(&mut errors, &at, &table.dataset, &columns)
                    else {
                        continue;
                    };
                    for (column_index, column) in table.columns.iter().enumerate() {
                        expect(
                            &mut errors,
                            format!("{at}/columns/{column_index}/column"),
                            found,
                            &column.column,
                            Expect::Any,
                        );
                    }
                }
                Component::Status(status) => check_value(&mut errors, &at, &status.value, &columns),
                Component::CustomPage(_) => {}
            }
        }
        errors
    }

    /// Checks the components against a write's results: the columns
    /// ([`Self::check_component_columns`]), and, when they are all right, that no series of a
    /// time series or bar chart, and no KPI trend, has two rows at one x. Rows with a `null` x
    /// are not points and are not compared. Two rows at one x would be two values for one
    /// point; combining them is the query's work, not the component's. A KPI's or a status'
    /// cell must be a row of this run's result ([`SpecProblem::RowMissing`]), as a custom page's
    /// reference must be.
    #[must_use]
    pub fn check_components(&self, results: &[DatasetResult]) -> Vec<SpecError> {
        let result = |name: &str| results.iter().find(|result| result.dataset == name);
        let errors =
            self.check_component_columns(|name| result(name).map(|found| found.columns.as_slice()));
        if !errors.is_empty() {
            return errors;
        }
        let mut errors = Vec::new();
        for (index, component) in self.components.iter().enumerate() {
            let at = format!("/spec/components/{index}");
            if let Component::Kpi(Kpi { value, .. }) | Component::Status(Status { value, .. }) =
                component
                && let ValueSource::Cell { dataset, row, .. } = value
                && let Some(result) = result(dataset)
                && usize::try_from(*row).is_ok_and(|row| row >= result.rows.len())
            {
                errors.push(SpecError::new(
                    format!("{at}/value/row"),
                    SpecProblem::RowMissing,
                ));
            }
            match component {
                Component::TimeSeries(chart) | Component::Bar(chart) => {
                    let Some(result) = result(&chart.dataset) else {
                        continue;
                    };
                    let drawn = chart.series.as_deref().map_or(chart.y.len(), |series| {
                        series_values(result, series).saturating_mul(chart.y.len())
                    });
                    if drawn > MAX_SERIES {
                        errors.push(SpecError::new(
                            format!("{at}/series"),
                            SpecProblem::SeriesTooMany { max: MAX_SERIES },
                        ));
                    }
                    if has_duplicate_x(result, &chart.x, chart.series.as_deref()) {
                        errors.push(SpecError::new(format!("{at}/x"), SpecProblem::XDuplicate));
                    }
                }
                Component::Kpi(Kpi {
                    trend: Some(trend), ..
                }) => {
                    if let Some(result) = result(&trend.dataset)
                        && has_duplicate_x(result, &trend.x, None)
                    {
                        errors.push(SpecError::new(
                            format!("{at}/trend/x"),
                            SpecProblem::XDuplicate,
                        ));
                    }
                }
                _ => {}
            }
        }
        errors
    }
}

/// How many series the `series` column of `result` splits its rows into: its distinct values,
/// `null` counted as one value, since those rows are drawn too.
fn series_values(result: &DatasetResult, series: &str) -> usize {
    let Some(index) = result
        .columns
        .iter()
        .position(|column| column.name == series)
    else {
        return 0;
    };
    result
        .rows
        .iter()
        .map(|row| row.get(index).and_then(Value::as_str))
        .collect::<BTreeSet<_>>()
        .len()
}

/// Whether two rows of `result` with a non-null `x` share their `x` and their `series` value.
fn has_duplicate_x(result: &DatasetResult, x: &str, series: Option<&str>) -> bool {
    let position = |name: &str| result.columns.iter().position(|column| column.name == name);
    let Some(x) = position(x) else {
        return false;
    };
    let series = series.and_then(position);
    let mut seen = BTreeSet::new();
    for row in &result.rows {
        let Some(Value::String(x_value)) = row.get(x) else {
            continue;
        };
        let series_value = series
            .and_then(|index| row.get(index))
            .and_then(Value::as_str);
        if !seen.insert((series_value, x_value.as_str())) {
            return true;
        }
    }
    false
}

/// The column types a component field accepts.
#[derive(Clone, Copy)]
enum Expect {
    Time,
    Number,
    Text,
    /// A bar chart's x: text or time.
    Category,
    Any,
}

/// The result columns of `dataset`, or a [`SpecProblem::DatasetUnknown`] at `{at}/dataset`.
fn result_columns<'a>(
    errors: &mut Vec<SpecError>,
    at: &str,
    dataset: &str,
    columns: &impl Fn(&str) -> Option<&'a [ResultColumn]>,
) -> Option<&'a [ResultColumn]> {
    let found = columns(dataset);
    if found.is_none() {
        errors.push(SpecError::new(
            format!("{at}/dataset"),
            SpecProblem::DatasetUnknown,
        ));
    }
    found
}

fn check_value<'a>(
    errors: &mut Vec<SpecError>,
    at: &str,
    value: &ValueSource,
    columns: &impl Fn(&str) -> Option<&'a [ResultColumn]>,
) {
    if let ValueSource::Cell {
        dataset, column, ..
    } = value
    {
        let at = format!("{at}/value");
        if let Some(found) = result_columns(errors, &at, dataset, columns) {
            expect(
                errors,
                format!("{at}/column"),
                found,
                column,
                Expect::Number,
            );
        }
    }
}

fn expect(
    errors: &mut Vec<SpecError>,
    path: String,
    columns: &[ResultColumn],
    name: &str,
    expected: Expect,
) {
    let Some(column) = columns.iter().find(|column| column.name == name) else {
        errors.push(SpecError::new(path, SpecProblem::ColumnUnknown));
        return;
    };
    let problem = match (expected, column.kind) {
        (Expect::Any, _)
        | (Expect::Time, ColumnKind::Time)
        | (Expect::Number, ColumnKind::Number)
        | (Expect::Text, ColumnKind::Text)
        | (Expect::Category, ColumnKind::Text | ColumnKind::Time) => return,
        (Expect::Time, _) => SpecProblem::ColumnNotTime,
        (Expect::Number, _) => SpecProblem::ColumnNotNumber,
        (Expect::Text, _) => SpecProblem::ColumnNotText,
        (Expect::Category, _) => SpecProblem::ColumnNotCategory,
    };
    errors.push(SpecError::new(path, problem));
}

/// Checks the component list without results (`Spec::validate`): its length, at most one custom
/// page, every title and unit, every name, and every reference.
pub(super) fn validate_components(spec: &Spec, errors: &mut Vec<SpecError>) {
    if spec.components.len() > MAX_COMPONENTS {
        errors.push(SpecError::new(
            "/spec/components",
            SpecProblem::TooManyComponents {
                max: MAX_COMPONENTS,
            },
        ));
    }
    let mut custom_pages = 0_usize;
    for (index, component) in spec.components.iter().enumerate() {
        let at = format!("/spec/components/{index}");
        match component {
            Component::TimeSeries(chart) | Component::Bar(chart) => {
                validate_chart(spec, errors, &at, chart);
            }
            Component::Kpi(kpi) => {
                check_text(errors, &at, "title", &kpi.title, TextRule::TITLE);
                validate_value(spec, errors, &at, &kpi.value);
                if let Some(trend) = &kpi.trend {
                    let at = format!("{at}/trend");
                    dataset(spec, errors, &at, &trend.dataset);
                    let mut seen = Columns::default();
                    seen.check(errors, format!("{at}/x"), &trend.x);
                    seen.check(errors, format!("{at}/y"), &trend.y);
                }
                if let Some(compare) = &kpi.compare {
                    computation(spec, errors, format!("{at}/compare"), compare);
                }
                if let Some(unit) = &kpi.unit {
                    check_text(errors, &at, "unit", unit, TextRule::UNIT);
                }
            }
            Component::Table(table) => {
                check_text(errors, &at, "title", &table.title, TextRule::TITLE);
                dataset(spec, errors, &at, &table.dataset);
                if table.columns.is_empty() || table.columns.len() > MAX_TABLE_COLUMNS {
                    errors.push(SpecError::new(
                        format!("{at}/columns"),
                        SpecProblem::TableColumnsCount {
                            max: MAX_TABLE_COLUMNS,
                        },
                    ));
                }
                let mut seen = Columns::default();
                for (column_index, column) in table.columns.iter().enumerate() {
                    let at = format!("{at}/columns/{column_index}");
                    seen.check(errors, format!("{at}/column"), &column.column);
                    if let Some(unit) = &column.unit {
                        check_text(errors, &at, "unit", unit, TextRule::UNIT);
                    }
                }
            }
            Component::Status(status) => {
                check_text(errors, &at, "title", &status.title, TextRule::TITLE);
                validate_value(spec, errors, &at, &status.value);
                thresholds(spec, errors, &at, &status.thresholds, 1);
                if let Some(unit) = &status.unit {
                    check_text(errors, &at, "unit", unit, TextRule::UNIT);
                }
            }
            Component::CustomPage(_) => {
                custom_pages += 1;
                if custom_pages > 1 {
                    errors.push(SpecError::new(at, SpecProblem::CustomPageDuplicate));
                }
            }
        }
    }
}

/// Checks that the list and the page agree (`validate_version`): the list has at least one item
/// (a version without a list and with a page counts its page as one, [`Spec::layout`]), and it
/// has a custom page item exactly when there is a page.
pub(super) fn validate_page_item(spec: &Spec, has_page: bool, errors: &mut Vec<SpecError>) {
    if spec.components.is_empty() {
        if !has_page {
            errors.push(SpecError::new(
                "/spec/components",
                SpecProblem::NoComponents,
            ));
        }
        return;
    }
    let custom_page = spec
        .components
        .iter()
        .position(|component| matches!(component, Component::CustomPage(_)));
    match (custom_page, has_page) {
        (Some(index), false) => errors.push(SpecError::new(
            format!("/spec/components/{index}"),
            SpecProblem::CustomPageWithoutPage,
        )),
        (None, true) => errors.push(SpecError::new("/page", SpecProblem::PageWithoutCustomPage)),
        _ => {}
    }
}

fn validate_chart(spec: &Spec, errors: &mut Vec<SpecError>, at: &str, chart: &SeriesChart) {
    check_text(errors, at, "title", &chart.title, TextRule::TITLE);
    dataset(spec, errors, at, &chart.dataset);
    let mut seen = Columns::default();
    seen.check(errors, format!("{at}/x"), &chart.x);
    if let Some(series) = &chart.series {
        seen.check(errors, format!("{at}/series"), series);
    }
    if chart.y.is_empty() || chart.y.len() > MAX_SERIES {
        errors.push(SpecError::new(
            format!("{at}/y"),
            SpecProblem::YColumnsCount { max: MAX_SERIES },
        ));
    }
    for (index, y) in chart.y.iter().enumerate() {
        seen.check(errors, format!("{at}/y/{index}"), y);
    }
    if let Some(unit) = &chart.unit {
        check_text(errors, at, "unit", unit, TextRule::UNIT);
    }
    thresholds(spec, errors, at, &chart.thresholds, 0);
}

fn validate_value(spec: &Spec, errors: &mut Vec<SpecError>, at: &str, value: &ValueSource) {
    let at = format!("{at}/value");
    match value {
        ValueSource::Cell {
            dataset: name,
            column,
            row,
        } => {
            let found = dataset(spec, errors, &at, name);
            Columns::default().check(errors, format!("{at}/column"), column);
            if let Some(found) = found
                && *row >= found.max_rows
            {
                errors.push(SpecError::new(
                    format!("{at}/row"),
                    SpecProblem::RowOutOfRange,
                ));
            }
        }
        ValueSource::Computation { name } => {
            computation(spec, errors, format!("{at}/name"), name);
        }
    }
}

/// The dataset `name` names, or a [`SpecProblem::DatasetUnknown`] at `{at}/dataset`.
fn dataset<'a>(
    spec: &'a Spec,
    errors: &mut Vec<SpecError>,
    at: &str,
    name: &str,
) -> Option<&'a Dataset> {
    let found = spec.dataset(name);
    if found.is_none() {
        errors.push(SpecError::new(
            format!("{at}/dataset"),
            SpecProblem::DatasetUnknown,
        ));
    }
    found
}

fn computation(spec: &Spec, errors: &mut Vec<SpecError>, path: String, name: &str) {
    if !spec
        .computations
        .iter()
        .any(|computation| computation.name == name)
    {
        errors.push(SpecError::new(path, SpecProblem::ComputationUnknown));
    }
}

/// Checks a component's thresholds: `min` to [`MAX_THRESHOLDS`] of them, each a number
/// constant, strictly increasing.
fn thresholds(spec: &Spec, errors: &mut Vec<SpecError>, at: &str, names: &[String], min: usize) {
    if names.len() < min || names.len() > MAX_THRESHOLDS {
        errors.push(SpecError::new(
            format!("{at}/thresholds"),
            SpecProblem::ThresholdsCount {
                max: MAX_THRESHOLDS,
            },
        ));
    }
    let mut values = Vec::with_capacity(names.len());
    for (index, name) in names.iter().enumerate() {
        let path = format!("{at}/thresholds/{index}");
        match spec
            .constants
            .iter()
            .find(|constant| constant.name == *name)
        {
            None => errors.push(SpecError::new(path, SpecProblem::ConstantUnknown)),
            Some(Constant {
                value: ConstantValue::Number(number),
                ..
            }) => values.push(*number),
            Some(_) => errors.push(SpecError::new(path, SpecProblem::ThresholdNotNumber)),
        }
    }
    let ascending = values
        .windows(2)
        .all(|pair| pair[0].partial_cmp(&pair[1]) == Some(Ordering::Less));
    if values.len() == names.len() && !ascending {
        errors.push(SpecError::new(
            format!("{at}/thresholds"),
            SpecProblem::ThresholdsNotAscending,
        ));
    }
}

/// The columns one component names: each a column name, none twice.
#[derive(Default)]
struct Columns<'a>(BTreeSet<&'a str>);

impl<'a> Columns<'a> {
    fn check(&mut self, errors: &mut Vec<SpecError>, path: String, name: &'a str) {
        if !is_column_name(name) {
            errors.push(SpecError::new(path, SpecProblem::ColumnInvalid));
        } else if !self.0.insert(name) {
            errors.push(SpecError::new(path, SpecProblem::ColumnDuplicate));
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::api::dashboard::spec::validate_version;

    const PAGE: &str = "<div></div>";

    fn component_json() -> Vec<Value> {
        vec![
            json!({"kind": "time_series", "title": "Usage per day", "dataset": "usage", "x": "time",
                   "y": ["usage"], "series": "device_id", "unit": "kWh", "thresholds": ["warn", "alarm"]}),
            json!({"kind": "bar", "title": "Daily energy", "dataset": "daily", "x": "day", "y": ["kwh"],
                   "unit": "kWh"}),
            json!({"kind": "kpi", "title": "This week", "value": {"kind": "computation", "name": "week_total"},
                   "trend": {"dataset": "usage", "x": "time", "y": "usage"}, "compare": "first_day",
                   "unit": "kWh"}),
            json!({"kind": "table", "title": "Days", "dataset": "daily",
                   "columns": [{"column": "day"}, {"column": "kwh", "unit": "kWh"}]}),
            json!({"kind": "status", "title": "First day",
                   "value": {"kind": "cell", "dataset": "daily", "column": "kwh", "row": 0},
                   "thresholds": ["warn", "alarm"], "unit": "kWh"}),
            json!({"kind": "custom_page"}),
        ]
    }

    fn spec_json() -> Value {
        json!({
            "datasets": [
                {
                    "name": "daily",
                    "title": "Daily energy",
                    "description": "Energy per day over the last week.",
                    "max_rows": 8,
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
                {"name": "warn", "title": "Warning", "value": 30.0, "unit": "kWh"},
                {"name": "alarm", "title": "Alarm", "value": 40.0, "unit": "kWh"},
                {"name": "line", "title": "Line", "value": "Line 2"}
            ],
            "components": component_json()
        })
    }

    fn spec() -> Spec {
        serde_json::from_value(spec_json()).expect("spec")
    }

    /// The spec with only `component` in its list.
    fn only(component: Value) -> Spec {
        let mut value = spec_json();
        value["components"] = json!([component]);
        serde_json::from_value(value).expect("spec")
    }

    fn problems(spec: &Spec, page: Option<&str>) -> Vec<(String, SpecProblem)> {
        validate_version("Energy", None, spec, page)
            .into_iter()
            .map(|error| (error.path, error.problem))
            .collect()
    }

    fn one(path: &str, problem: SpecProblem) -> Vec<(String, SpecProblem)> {
        vec![(path.to_owned(), problem)]
    }

    fn result(
        dataset: &str,
        columns: &[(&str, ColumnKind)],
        rows: Vec<Vec<Value>>,
    ) -> DatasetResult {
        let columns: Vec<Value> = columns
            .iter()
            .map(|(name, kind)| json!({"name": name, "type": kind}))
            .collect();
        serde_json::from_value(json!({
            "dataset": dataset,
            "columns": columns,
            "row_count": rows.len(),
            "rows": rows,
            "meta": {
                "queried_at": "2026-11-01T01:02:03Z",
                "duration_ms": 5,
                "source": {"kind": "sql", "queryable_from": {
                    "tenant_telemetry": "2026-01-01T00:00:00Z",
                    "tenant_telemetry_1m": "2026-01-01T00:00:00Z",
                    "tenant_telemetry_1h": "2026-01-01T00:00:00Z"
                }}
            }
        }))
        .expect("result")
    }

    fn results() -> Vec<DatasetResult> {
        vec![
            result(
                "daily",
                &[("day", ColumnKind::Time), ("kwh", ColumnKind::Number)],
                vec![
                    vec![json!("2026-10-26"), json!(12.5)],
                    vec![json!("2026-10-27"), json!(13.0)],
                ],
            ),
            result(
                "usage",
                &[
                    ("device_id", ColumnKind::Text),
                    ("time", ColumnKind::Time),
                    ("usage", ColumnKind::Number),
                ],
                vec![
                    vec![json!("m1"), json!("2026-10-26T00:00:00+08:00"), json!(1.5)],
                    vec![json!("m2"), json!("2026-10-27T00:00:00+08:00"), json!(2.5)],
                    vec![json!("m1"), json!("2026-10-27T00:00:00+08:00"), json!(1.0)],
                ],
            ),
        ]
    }

    #[test]
    fn every_kind_passes_alone_and_together() {
        let spec = spec();
        assert_eq!(problems(&spec, Some(PAGE)), vec![]);
        assert_eq!(serde_json::to_value(&spec).expect("serialize"), spec_json());
        for component in component_json() {
            let custom = component["kind"] == "custom_page";
            let spec = only(component.clone());
            let page = custom.then_some(PAGE);
            assert_eq!(problems(&spec, page), vec![], "{component}");
        }
        // The trend has no series, so it needs one row per time: give it its own dataset.
        let mut results = results();
        results[1].rows.truncate(2);
        assert_eq!(spec.check_components(&results), vec![]);
    }

    #[test]
    fn kinds_and_fields_are_closed() {
        for component in component_json() {
            let mut extra = component.clone();
            extra["color"] = json!("#ff0000");
            assert!(
                serde_json::from_value::<Component>(extra).is_err(),
                "{component}"
            );
        }
        let mut status = component_json()[4].clone();
        status["value"]["style"] = json!("x");
        assert!(serde_json::from_value::<Component>(status).is_err());
        let mut kpi = component_json()[2].clone();
        kpi["trend"]["url"] = json!("https://example.com");
        assert!(serde_json::from_value::<Component>(kpi).is_err());
        let mut table = component_json()[3].clone();
        table["columns"][0]["html"] = json!("<b>");
        assert!(serde_json::from_value::<Component>(table).is_err());
        for kind in ["pie", "html", "Time_series", ""] {
            assert!(serde_json::from_value::<Component>(json!({"kind": kind})).is_err());
        }
        let mut spec = spec_json();
        spec["components"][0]["kind"] = json!("pie");
        assert!(serde_json::from_value::<Spec>(spec).is_err());
    }

    #[test]
    fn parse_names_each_problem() {
        let at = "/spec/components/0";
        let parse = |value: Value| {
            Component::parse(&value, at).map_err(|errors| {
                errors
                    .into_iter()
                    .map(|error| (error.path, error.problem))
                    .collect::<Vec<_>>()
            })
        };
        for component in component_json() {
            assert_eq!(
                parse(component.clone()),
                Ok(serde_json::from_value::<Component>(component).expect("component"))
            );
        }
        assert_eq!(
            parse(json!("time_series")),
            Err(one(at, SpecProblem::ComponentInvalid))
        );
        for kind in [
            json!({"kind": "pie"}),
            json!({"title": "x"}),
            json!({"kind": 3}),
        ] {
            assert_eq!(
                parse(kind),
                Err(one(
                    "/spec/components/0/kind",
                    SpecProblem::ComponentKindUnknown
                ))
            );
        }
        let mut chart = component_json()[0].clone();
        chart["color"] = json!("red");
        chart["Weird Key/"] = json!(1);
        let mut found = parse(chart).expect_err("unknown fields");
        found.sort_by(|left, right| left.0.cmp(&right.0));
        assert_eq!(
            found,
            vec![
                (at.to_owned(), SpecProblem::ComponentFieldUnknown),
                (
                    "/spec/components/0/color".to_owned(),
                    SpecProblem::ComponentFieldUnknown
                ),
            ]
        );
        let mut chart = component_json()[0].clone();
        chart["y"] = json!("usage");
        chart["series"] = json!(3);
        let mut found = parse(chart).expect_err("invalid fields");
        found.sort_by(|left, right| left.0.cmp(&right.0));
        assert_eq!(
            found,
            vec![
                (
                    "/spec/components/0/series".to_owned(),
                    SpecProblem::ComponentFieldInvalid
                ),
                (
                    "/spec/components/0/y".to_owned(),
                    SpecProblem::ComponentFieldInvalid
                ),
            ]
        );
        let mut status = component_json()[4].clone();
        status["value"] = json!({"kind": "cell", "dataset": "daily"});
        assert_eq!(
            parse(status),
            Err(one(
                "/spec/components/0/value",
                SpecProblem::ComponentFieldInvalid
            ))
        );
        assert_eq!(
            parse(json!({"kind": "custom_page", "html": "<p></p>"})),
            Err(one(
                "/spec/components/0/html",
                SpecProblem::ComponentFieldUnknown
            ))
        );
        for component in component_json() {
            let kind =
                ComponentKind::parse(component["kind"].as_str().expect("kind")).expect("kind");
            let (required, _) = kind.field_names();
            for field in required {
                let mut missing = component.clone();
                missing.as_object_mut().expect("object").remove(field);
                assert_eq!(
                    parse(missing),
                    Err(one(
                        &format!("/spec/components/0/{field}"),
                        SpecProblem::ComponentFieldMissing
                    )),
                    "{field} of {}",
                    kind.as_str()
                );
            }
        }
    }

    #[test]
    fn the_field_table_matches_the_types() {
        let examples = component_json();
        assert_eq!(examples.len(), ComponentKind::ALL.len());
        for (kind, example) in ComponentKind::ALL.into_iter().zip(&examples) {
            assert_eq!(example["kind"], kind.as_str());
            assert_eq!(
                serde_json::to_value(kind).expect("kind"),
                json!(kind.as_str())
            );
            let parsed: Component = serde_json::from_value(example.clone()).expect("example");
            assert_eq!(parsed.kind(), kind);
            let (required, optional) = kind.field_names();
            // The examples set every field of every type (the bar shares the time series' type),
            // so `parse` accepting them shows the table has every field the types have.
            let keys: Vec<&str> = example
                .as_object()
                .expect("object")
                .keys()
                .map(String::as_str)
                .filter(|key| *key != "kind")
                .collect();
            for key in &keys {
                assert!(
                    required.contains(key) || optional.contains(key),
                    "{key} of {}",
                    kind.as_str()
                );
            }
            for field in &required {
                assert!(keys.contains(field), "{field} of {}", kind.as_str());
            }
            if kind != ComponentKind::Bar {
                assert_eq!(
                    keys.len(),
                    required.len() + optional.len(),
                    "{}",
                    kind.as_str()
                );
            }
            for field in required {
                let mut value = example.clone();
                value.as_object_mut().expect("object").remove(field);
                assert!(
                    serde_json::from_value::<Component>(value).is_err(),
                    "{field} is required in {}",
                    kind.as_str()
                );
            }
            for field in optional {
                let mut value = example.clone();
                value.as_object_mut().expect("object").remove(field);
                assert!(
                    serde_json::from_value::<Component>(value).is_ok(),
                    "{field} is optional in {}",
                    kind.as_str()
                );
            }
        }
    }

    #[test]
    fn references_must_exist() {
        let mut chart = component_json()[0].clone();
        chart["dataset"] = json!("nope");
        chart["thresholds"] = json!(["warn", "missing"]);
        assert_eq!(
            problems(&only(chart), None),
            vec![
                (
                    "/spec/components/0/dataset".to_owned(),
                    SpecProblem::DatasetUnknown
                ),
                (
                    "/spec/components/0/thresholds/1".to_owned(),
                    SpecProblem::ConstantUnknown
                ),
            ]
        );
        let mut kpi = component_json()[2].clone();
        kpi["value"] = json!({"kind": "computation", "name": "warn"});
        kpi["compare"] = json!("nope");
        kpi["trend"]["dataset"] = json!("nope");
        assert_eq!(
            problems(&only(kpi), None),
            vec![
                (
                    "/spec/components/0/value/name".to_owned(),
                    SpecProblem::ComputationUnknown
                ),
                (
                    "/spec/components/0/trend/dataset".to_owned(),
                    SpecProblem::DatasetUnknown
                ),
                (
                    "/spec/components/0/compare".to_owned(),
                    SpecProblem::ComputationUnknown
                ),
            ]
        );
        let mut status = component_json()[4].clone();
        status["value"] = json!({"kind": "cell", "dataset": "nope", "column": "kwh", "row": 0});
        assert_eq!(
            problems(&only(status), None),
            one(
                "/spec/components/0/value/dataset",
                SpecProblem::DatasetUnknown
            )
        );
        let mut table = component_json()[3].clone();
        table["dataset"] = json!("week_total");
        assert_eq!(
            problems(&only(table), None),
            one("/spec/components/0/dataset", SpecProblem::DatasetUnknown)
        );
    }

    #[test]
    fn thresholds_are_number_constants_from_low_to_high() {
        let with = |thresholds: Value, kind: usize| {
            let mut component = component_json()[kind].clone();
            component["thresholds"] = thresholds;
            problems(&only(component), None)
        };
        for kind in [0, 1, 4] {
            assert_eq!(with(json!(["warn", "alarm"]), kind), vec![]);
            assert_eq!(
                with(json!(["alarm", "warn"]), kind),
                one(
                    "/spec/components/0/thresholds",
                    SpecProblem::ThresholdsNotAscending
                )
            );
            assert_eq!(
                with(json!(["warn", "warn"]), kind),
                one(
                    "/spec/components/0/thresholds",
                    SpecProblem::ThresholdsNotAscending
                )
            );
            assert_eq!(
                with(json!(["warn", "line"]), kind),
                one(
                    "/spec/components/0/thresholds/1",
                    SpecProblem::ThresholdNotNumber
                )
            );
            assert_eq!(
                with(json!(["warn", "alarm", "warn", "alarm", "warn"]), kind)[0],
                (
                    "/spec/components/0/thresholds".to_owned(),
                    SpecProblem::ThresholdsCount {
                        max: MAX_THRESHOLDS
                    }
                )
            );
        }
        // A chart needs none; a status at least one.
        assert_eq!(with(json!([]), 0), vec![]);
        assert_eq!(
            with(json!([]), 4),
            one(
                "/spec/components/0/thresholds",
                SpecProblem::ThresholdsCount {
                    max: MAX_THRESHOLDS
                }
            )
        );
    }

    #[test]
    fn columns_counts_rows_and_texts_are_bounded() {
        let mut chart = component_json()[0].clone();
        chart["y"] = json!([]);
        assert_eq!(
            problems(&only(chart.clone()), None),
            one(
                "/spec/components/0/y",
                SpecProblem::YColumnsCount { max: MAX_SERIES }
            )
        );
        chart["y"] = json!(
            (0..=MAX_SERIES)
                .map(|index| format!("y{index}"))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            problems(&only(chart.clone()), None),
            one(
                "/spec/components/0/y",
                SpecProblem::YColumnsCount { max: MAX_SERIES }
            )
        );
        chart["y"] = json!(["usage", "time", "usage", "bad name", "device_id"]);
        assert_eq!(
            problems(&only(chart), None),
            vec![
                (
                    "/spec/components/0/y/1".to_owned(),
                    SpecProblem::ColumnDuplicate
                ),
                (
                    "/spec/components/0/y/2".to_owned(),
                    SpecProblem::ColumnDuplicate
                ),
                (
                    "/spec/components/0/y/3".to_owned(),
                    SpecProblem::ColumnInvalid
                ),
                (
                    "/spec/components/0/y/4".to_owned(),
                    SpecProblem::ColumnDuplicate
                ),
            ]
        );
        let mut table = component_json()[3].clone();
        table["columns"] = json!([]);
        assert_eq!(
            problems(&only(table.clone()), None),
            one(
                "/spec/components/0/columns",
                SpecProblem::TableColumnsCount {
                    max: MAX_TABLE_COLUMNS
                }
            )
        );
        table["columns"] = json!(
            (0..=MAX_TABLE_COLUMNS)
                .map(|index| json!({"column": format!("c{index}")}))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            problems(&only(table.clone()), None),
            one(
                "/spec/components/0/columns",
                SpecProblem::TableColumnsCount {
                    max: MAX_TABLE_COLUMNS
                }
            )
        );
        table["columns"] = json!([{"column": "day"}, {"column": "day", "unit": ""}]);
        assert_eq!(
            problems(&only(table), None),
            vec![
                (
                    "/spec/components/0/columns/1/column".to_owned(),
                    SpecProblem::ColumnDuplicate
                ),
                (
                    "/spec/components/0/columns/1/unit".to_owned(),
                    SpecProblem::UnitInvalid { max_chars: 24 }
                ),
            ]
        );
        let mut status = component_json()[4].clone();
        status["value"]["row"] = json!(8);
        assert_eq!(
            problems(&only(status.clone()), None),
            one("/spec/components/0/value/row", SpecProblem::RowOutOfRange)
        );
        status["value"]["row"] = json!(7);
        assert_eq!(problems(&only(status), None), vec![]);
        let mut kpi = component_json()[2].clone();
        kpi["title"] = json!("t".repeat(121));
        kpi["unit"] = json!("u".repeat(25));
        kpi["trend"]["y"] = json!("time");
        assert_eq!(
            problems(&only(kpi), None),
            vec![
                (
                    "/spec/components/0/title".to_owned(),
                    SpecProblem::TitleInvalid { max_chars: 120 }
                ),
                (
                    "/spec/components/0/trend/y".to_owned(),
                    SpecProblem::ColumnDuplicate
                ),
                (
                    "/spec/components/0/unit".to_owned(),
                    SpecProblem::UnitInvalid { max_chars: 24 }
                ),
            ]
        );
        let mut value = spec_json();
        value["components"] = json!(vec![component_json()[1].clone(); MAX_COMPONENTS + 1]);
        let spec: Spec = serde_json::from_value(value).expect("spec");
        assert_eq!(
            problems(&spec, None),
            one(
                "/spec/components",
                SpecProblem::TooManyComponents {
                    max: MAX_COMPONENTS
                }
            )
        );
        let mut value = spec_json();
        value["components"] = json!(vec![component_json()[1].clone(); MAX_COMPONENTS]);
        let spec: Spec = serde_json::from_value(value).expect("spec");
        assert_eq!(problems(&spec, None), vec![]);
    }

    #[test]
    fn the_list_and_the_page_agree() {
        // A version written before components: no list, a page.
        let mut legacy = spec();
        legacy.components.clear();
        assert_eq!(problems(&legacy, Some(PAGE)), vec![]);
        assert_eq!(
            legacy.layout(true).as_ref(),
            &[Component::CustomPage(CustomPage {})]
        );
        assert_eq!(
            problems(&legacy, None),
            one("/spec/components", SpecProblem::NoComponents)
        );
        assert!(legacy.layout(false).is_empty());
        let spec = spec();
        assert_eq!(spec.layout(true).as_ref(), spec.components.as_slice());
        assert_eq!(
            problems(&spec, None),
            one("/spec/components/5", SpecProblem::CustomPageWithoutPage)
        );
        let chart = only(component_json()[1].clone());
        assert_eq!(problems(&chart, None), vec![]);
        assert_eq!(
            problems(&chart, Some(PAGE)),
            one("/page", SpecProblem::PageWithoutCustomPage)
        );
        let mut value = spec_json();
        value["components"] = json!([{"kind": "custom_page"}, {"kind": "custom_page"}]);
        let two: Spec = serde_json::from_value(value).expect("spec");
        assert_eq!(
            problems(&two, Some(PAGE)),
            one("/spec/components/1", SpecProblem::CustomPageDuplicate)
        );
        assert_eq!(
            problems(&only(json!({"kind": "custom_page"})), Some(" ")),
            one("/page", SpecProblem::PageEmpty)
        );
    }

    #[test]
    fn column_types_must_suit_the_component() {
        let results = results();
        let columns = |name: &str| {
            results
                .iter()
                .find(|result| result.dataset == name)
                .map(|result| result.columns.as_slice())
        };
        let check = |component: Value| {
            only(component)
                .check_component_columns(columns)
                .into_iter()
                .map(|error| (error.path, error.problem))
                .collect::<Vec<_>>()
        };
        for component in component_json() {
            assert_eq!(check(component.clone()), vec![], "{component}");
        }
        let mut chart = component_json()[0].clone();
        chart["x"] = json!("usage");
        chart["y"] = json!(["device_id", "nope"]);
        chart["series"] = json!("time");
        assert_eq!(
            check(chart),
            vec![
                (
                    "/spec/components/0/x".to_owned(),
                    SpecProblem::ColumnNotTime
                ),
                (
                    "/spec/components/0/y/0".to_owned(),
                    SpecProblem::ColumnNotNumber
                ),
                (
                    "/spec/components/0/y/1".to_owned(),
                    SpecProblem::ColumnUnknown
                ),
                (
                    "/spec/components/0/series".to_owned(),
                    SpecProblem::ColumnNotText
                ),
            ]
        );
        let mut bar = component_json()[1].clone();
        bar["x"] = json!("kwh");
        assert_eq!(
            check(bar.clone()),
            one("/spec/components/0/x", SpecProblem::ColumnNotCategory)
        );
        bar["dataset"] = json!("usage");
        bar["x"] = json!("device_id");
        bar["y"] = json!(["usage"]);
        assert_eq!(check(bar), vec![]);
        let mut kpi = component_json()[2].clone();
        kpi["value"] = json!({"kind": "cell", "dataset": "daily", "column": "day", "row": 0});
        kpi["trend"] = json!({"dataset": "daily", "x": "kwh", "y": "day"});
        assert_eq!(
            check(kpi),
            vec![
                (
                    "/spec/components/0/value/column".to_owned(),
                    SpecProblem::ColumnNotNumber
                ),
                (
                    "/spec/components/0/trend/x".to_owned(),
                    SpecProblem::ColumnNotTime
                ),
                (
                    "/spec/components/0/trend/y".to_owned(),
                    SpecProblem::ColumnNotNumber
                ),
            ]
        );
        let mut table = component_json()[3].clone();
        table["columns"] = json!([{"column": "kwh"}, {"column": "day"}, {"column": "usage"}]);
        assert_eq!(
            check(table),
            one(
                "/spec/components/0/columns/2/column",
                SpecProblem::ColumnUnknown
            )
        );
        // A dataset without a result is reported, never passed over.
        let only_usage = |name: &str| columns(name).filter(|_| name == "usage");
        assert_eq!(
            only(component_json()[4].clone())
                .check_component_columns(only_usage)
                .into_iter()
                .map(|error| (error.path, error.problem))
                .collect::<Vec<_>>(),
            one(
                "/spec/components/0/value/dataset",
                SpecProblem::DatasetUnknown
            )
        );
    }

    #[test]
    fn one_series_has_one_row_per_x() {
        let check = |spec: &Spec, results: &[DatasetResult]| {
            spec.check_components(results)
                .into_iter()
                .map(|error| (error.path, error.problem))
                .collect::<Vec<_>>()
        };
        let chart = only(component_json()[0].clone());
        let kpi = only(component_json()[2].clone());
        // Two meters at one time are two series of the chart, but two rows at one time of the
        // trend, which has no series.
        let results = results();
        assert_eq!(check(&chart, &results), vec![]);
        assert_eq!(
            check(&kpi, &results),
            one("/spec/components/0/trend/x", SpecProblem::XDuplicate)
        );
        let mut twice = results.clone();
        twice[1].rows.push(vec![
            json!("m1"),
            json!("2026-10-26T00:00:00+08:00"),
            json!(9.0),
        ]);
        assert_eq!(
            check(&chart, &twice),
            one("/spec/components/0/x", SpecProblem::XDuplicate)
        );
        let bar = only(component_json()[1].clone());
        let mut nulls = results.clone();
        nulls[0].rows.push(vec![Value::Null, json!(1.0)]);
        nulls[0].rows.push(vec![Value::Null, json!(2.0)]);
        assert_eq!(
            check(&bar, &nulls),
            vec![],
            "rows without an x are not compared"
        );
        nulls[0].rows.push(vec![json!("2026-10-26"), json!(1.0)]);
        assert_eq!(
            check(&bar, &nulls),
            one("/spec/components/0/x", SpecProblem::XDuplicate)
        );
        // Six series are drawn (two meters, three y columns); a seventh meter is one series too
        // many, and so would be a third meter with the same columns. Rows without a meter are
        // a series too.
        let mut three_y = chart.clone();
        let Component::TimeSeries(series_chart) = &mut three_y.components[0] else {
            panic!("the first example is a time series");
        };
        series_chart.y = vec![
            "usage".to_owned(),
            "usage_a".to_owned(),
            "usage_b".to_owned(),
        ];
        let mut wide = results.clone();
        for name in ["usage_a", "usage_b"] {
            wide[1].columns.push(ResultColumn {
                name: name.to_owned(),
                kind: ColumnKind::Number,
            });
        }
        for row in &mut wide[1].rows {
            row.extend([json!(0.5), json!(0.25)]);
        }
        assert_eq!(check(&three_y, &wide), vec![]);
        let mut third = wide.clone();
        third[1].rows.push(vec![
            json!("m3"),
            json!("2026-10-26T00:00:00+08:00"),
            json!(1.0),
            json!(0.5),
            json!(0.25),
        ]);
        assert_eq!(
            check(&three_y, &third),
            one(
                "/spec/components/0/series",
                SpecProblem::SeriesTooMany { max: MAX_SERIES }
            )
        );
        let mut unnamed = wide;
        unnamed[1].rows.push(vec![
            Value::Null,
            json!("2026-10-26T00:00:00+08:00"),
            json!(1.0),
            json!(0.5),
            json!(0.25),
        ]);
        assert_eq!(
            check(&three_y, &unnamed),
            one(
                "/spec/components/0/series",
                SpecProblem::SeriesTooMany { max: MAX_SERIES }
            )
        );
        let mut many = results.clone();
        for meter in 3..=MAX_SERIES {
            many[1].rows.push(vec![
                json!(format!("m{meter}")),
                json!("2026-10-26T00:00:00+08:00"),
                json!(1.0),
            ]);
        }
        assert_eq!(check(&chart, &many), vec![], "{MAX_SERIES} meters, one y");
        many[1].rows.push(vec![
            json!("m_extra"),
            json!("2026-10-26T00:00:00+08:00"),
            json!(1.0),
        ]);
        assert_eq!(
            check(&chart, &many),
            one(
                "/spec/components/0/series",
                SpecProblem::SeriesTooMany { max: MAX_SERIES }
            )
        );
        // A status' cell must be a row of this run's result: `daily` has two rows.
        let mut status = only(component_json()[4].clone());
        assert_eq!(check(&status, &results), vec![]);
        let Component::Status(Status {
            value: ValueSource::Cell { row, .. },
            ..
        }) = &mut status.components[0]
        else {
            panic!("the status example reads a cell");
        };
        *row = 2;
        assert_eq!(
            check(&status, &results),
            one("/spec/components/0/value/row", SpecProblem::RowMissing)
        );
        let mut empty = results.clone();
        empty[0].rows.clear();
        let first = only(component_json()[4].clone());
        assert_eq!(
            check(&first, &empty),
            one("/spec/components/0/value/row", SpecProblem::RowMissing)
        );
        // Column problems come first and alone.
        let mut text = results;
        text[0].columns[1].kind = ColumnKind::Text;
        text[0].rows.push(vec![json!("2026-10-26"), json!("1")]);
        assert_eq!(
            check(&bar, &text),
            one("/spec/components/0/y/0", SpecProblem::ColumnNotNumber)
        );
    }

    #[test]
    fn the_schema_closes_each_kind_with_its_tag() {
        use utoipa::PartialSchema;

        assert_eq!(Component::name(), "DashboardComponent");
        let schema = serde_json::to_value(Component::schema()).expect("schema");
        let items = schema["oneOf"].as_array().expect("oneOf");
        assert_eq!(items.len(), ComponentKind::ALL.len());
        // Every example (every field of every kind set) must be accepted by its kind's object
        // as JSON Schema reads it: an object that lists `kind` among its properties, needs no
        // field the example lacks, and refuses any other field.
        for example in component_json() {
            let kind = example["kind"].as_str().expect("kind");
            let item = items
                .iter()
                .find(|item| item["properties"]["kind"]["enum"] == json!([kind]))
                .unwrap_or_else(|| panic!("no schema for {kind}: {schema}"));
            assert_eq!(item["type"], "object", "{kind}");
            assert_eq!(item["additionalProperties"], false, "{kind}");
            assert!(item.get("allOf").is_none(), "{kind}");
            let properties = item["properties"].as_object().expect("properties");
            let fields = example.as_object().expect("object");
            for key in fields.keys() {
                assert!(
                    properties.contains_key(key),
                    "{kind}: {key} not in the schema"
                );
            }
            for required in item["required"].as_array().expect("required") {
                let required = required.as_str().expect("name");
                assert!(fields.contains_key(required), "{kind}: {required} missing");
            }
            assert!(
                item["required"]
                    .as_array()
                    .expect("required")
                    .contains(&json!("kind"))
            );
        }
        let mut registered = Vec::new();
        <Component as ToSchema>::schemas(&mut registered);
        let names: BTreeSet<&str> = registered.iter().map(|(name, _)| name.as_str()).collect();
        assert!(names.contains("DashboardValueSource"), "{names:?}");
        assert!(names.contains("DashboardKpiTrend"), "{names:?}");
        assert!(names.contains("DashboardTableColumn"), "{names:?}");

        // The value source is closed the same way, and serde refuses what the schema refuses.
        assert_eq!(ValueSource::name(), "DashboardValueSource");
        let sources = serde_json::to_value(ValueSource::schema()).expect("schema");
        let sources = sources["oneOf"].as_array().expect("oneOf");
        let examples = [
            ValueSource::Cell {
                dataset: "meter_stats".to_owned(),
                column: "avg".to_owned(),
                row: 3,
            },
            ValueSource::Computation {
                name: "week_kwh".to_owned(),
            },
        ];
        assert_eq!(sources.len(), examples.len());
        for (example, item) in examples.iter().zip(sources) {
            let example = serde_json::to_value(example).expect("json");
            let fields = example.as_object().expect("object");
            assert_eq!(item["properties"]["kind"]["enum"], json!([example["kind"]]));
            assert_eq!(item["additionalProperties"], false);
            let properties = item["properties"].as_object().expect("properties");
            let required: BTreeSet<&str> = item["required"]
                .as_array()
                .expect("required")
                .iter()
                .map(|name| name.as_str().expect("name"))
                .collect();
            let keys: BTreeSet<&str> = fields.keys().map(String::as_str).collect();
            let listed: BTreeSet<&str> = properties.keys().map(String::as_str).collect();
            // Every field is required, so the schema lists exactly the fields serde writes.
            assert_eq!(keys, listed);
            assert_eq!(keys, required);
            let mut extra = example.clone();
            extra["unknown"] = json!(1);
            assert!(serde_json::from_value::<ValueSource>(extra).is_err());
        }
    }

    #[test]
    fn the_public_part_and_the_json_carry_the_list() {
        let spec = spec();
        let public = serde_json::to_value(spec.public()).expect("serialize");
        assert_eq!(public["components"], json!(component_json()));
        let mut legacy = spec;
        legacy.components.clear();
        assert!(
            serde_json::to_value(&legacy)
                .expect("json")
                .get("components")
                .is_none()
        );
        assert!(
            serde_json::to_value(legacy.public())
                .expect("json")
                .get("components")
                .is_none()
        );
    }
}
