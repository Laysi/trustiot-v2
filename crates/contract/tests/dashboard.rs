//! The dashboard contract's JSON fixtures and shared vectors (`tests/fixtures/dashboard/`): the
//! fixtures are written from the Rust types, so a change to a type fails here until the fixture
//! is rewritten (`CONTRACT_UPDATE_FIXTURES=1`) and reviewed; the frontend's sandbox protocol
//! test reads the same files. The formula and reference vectors are written by hand and run
//! here and in the sandbox runtime's tests.

mod common;

use std::collections::BTreeMap;
use std::fs;

use chrono::{DateTime, FixedOffset, Utc};
use contract::api::dashboard::RuntimeVersion;
use contract::api::dashboard::check::{
    CheckReport, CheckRequest, CheckVerdict, ContentKind, GeometryChannel, LiteralPlace, Locale,
    MarkReport, Problem, SizeLimit, TimeoutPhase,
};
use contract::api::dashboard::formula::{
    Cell, DatasetShape, Formula, FormulaData, FormulaErrorClass,
};
use contract::api::dashboard::names::Ref;
use contract::api::dashboard::public::{
    DashboardKind, DashboardSummary, DashboardView, Permissions, SavedBase, VersionContent,
    VersionOrigin,
};
use contract::api::dashboard::result::{
    ColumnKind, DatasetResult, FixedColumn, QueryablePeriod, ResultColumn, ResultMeta,
    STATISTICS_BUCKET_COLUMNS, STATISTICS_TOTAL_COLUMNS, SemanticRange, SemanticResolution,
    SourceMeta, USAGE_BUCKET_COLUMNS, USAGE_TOTAL_COLUMNS,
};
use contract::api::dashboard::spec::{
    Computation, Constant, ConstantValue, Dataset, DatasetSource, SemanticInterval,
    SemanticRequest, SemanticSeries, Spec, validate_version,
};
use contract::{DashboardId, DeviceId, UserId};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use common::{assert_json_fixture, fixture_path};

const METER: &str = "0190a5b2-7c3d-7e4f-8a1b-2c3d4e5f6a7b";
const LINE_B: &str = "0190a5b2-7c3d-7e4f-8a1b-2c3d4e5f6a7c";

fn time(text: &str) -> DateTime<FixedOffset> {
    DateTime::parse_from_rfc3339(text).expect("fixture time")
}

fn utc(text: &str) -> DateTime<Utc> {
    time(text).to_utc()
}

fn device(text: &str) -> DeviceId {
    DeviceId::from_uuid(Uuid::parse_str(text).expect("fixture uuid"))
}

fn series(device_id: &str, point: &str) -> SemanticSeries {
    SemanticSeries {
        device_id: device(device_id),
        point: point.to_owned(),
    }
}

fn spec() -> Spec {
    Spec {
        datasets: vec![
            Dataset {
                name: "daily_energy".to_owned(),
                title: "每日用電".to_owned(),
                description:
                    "Energy per local day over the last seven days, from the hourly cells."
                        .to_owned(),
                max_rows: 8,
                source: DatasetSource::Sql {
                    sql: "SELECT toDate(bucket, 'Asia/Taipei') AS day, round(sum(avg_value), 3) \
                          AS kwh FROM trustiot.tenant_telemetry_1h WHERE point = 'energy' AND \
                          bucket >= now() - INTERVAL 7 DAY GROUP BY day ORDER BY day"
                        .to_owned(),
                },
            },
            Dataset {
                name: "meter_stats".to_owned(),
                title: "Power over the last 7 days".to_owned(),
                description: "Statistics of the two meters' power over the last seven days."
                    .to_owned(),
                max_rows: 2,
                source: DatasetSource::TelemetryStatistics {
                    request: SemanticRequest {
                        series: vec![series(METER, "power"), series(LINE_B, "power")],
                        tz: "Asia/Taipei".to_owned(),
                        period: Some("last_7_days".to_owned()),
                        from: None,
                        to: None,
                        interval: SemanticInterval::Total,
                    },
                },
            },
            Dataset {
                name: "meter_usage".to_owned(),
                title: "Usage per day this week".to_owned(),
                description: "The main meter's energy usage per local day of this week.".to_owned(),
                max_rows: 7,
                source: DatasetSource::TelemetryUsage {
                    request: SemanticRequest {
                        series: vec![series(METER, "energy")],
                        tz: "Asia/Taipei".to_owned(),
                        period: None,
                        from: Some(time("2026-10-19T00:00:00+08:00")),
                        to: Some(time("2026-10-26T00:00:00+08:00")),
                        interval: SemanticInterval::Day1,
                    },
                },
            },
        ],
        computations: vec![
            Computation {
                name: "week_kwh".to_owned(),
                title: "This week".to_owned(),
                formula: "sum(meter_usage.usage)".to_owned(),
                unit: Some("kWh".to_owned()),
            },
            Computation {
                name: "carbon_kg".to_owned(),
                title: "Emissions".to_owned(),
                formula: "sum(meter_usage.usage) * 0.494".to_owned(),
                unit: Some("kgCO2e".to_owned()),
            },
            Computation {
                name: "first_day_share".to_owned(),
                title: "Share of the first day".to_owned(),
                formula: "daily_energy.kwh[0] / sum(daily_energy.kwh)".to_owned(),
                unit: None,
            },
        ],
        constants: vec![
            Constant {
                name: "target_kwh".to_owned(),
                title: "Weekly target".to_owned(),
                value: ConstantValue::Number(1200.0),
                unit: Some("kWh".to_owned()),
                note: Some("The target the user gave on 2026-10-26.".to_owned()),
            },
            Constant {
                name: "line_name".to_owned(),
                title: "Line".to_owned(),
                value: ConstantValue::Text("Line 2".to_owned()),
                unit: None,
                note: None,
            },
        ],
    }
}

/// A page written against runtime 1 (`frontend/sandbox/README.md`, 「頁面 API」).
const PAGE: &str = r##"<style>
  .kpi { font: 600 32px/1.2 system-ui; }
  .bar { fill: steelblue; }
</style>
<h1>每日用電 <span id="line"></span></h1>
<p>本週 <span id="week" class="kpi"></span> kWh，目標 <span id="target"></span> kWh</p>
<svg id="chart" width="480" height="240"></svg>
<script>
  tq.on("data", () => {
    tq.text(document.getElementById("line"), "k|line_name");
    tq.text(document.getElementById("week"), "f|week_kwh", ",.1f");
    tq.text(document.getElementById("target"), "k|target_kwh", ",.0f");
    const daily = tq.dataset("daily_energy");
    const svg = d3.select("#chart");
    svg.selectAll("*").remove();
    const x = d3.scaleBand(daily.rows.map((row) => row.day), [40, 470]).padding(0.2);
    const y = d3.scaleLinear([0, d3.max(daily.rows, (row) => row.kwh) ?? 0], [210, 10]).nice();
    tq.axis(svg.append("g").attr("transform", "translate(0,210)").node(), d3.axisBottom(x),
      { dataset: "daily_energy", column: "day", format: "%m/%d" });
    tq.axis(svg.append("g").attr("transform", "translate(40,0)").node(), d3.axisLeft(y),
      { dataset: "daily_energy", column: "kwh", format: ",.0f" });
    svg.append("g").selectAll("rect").data(daily.rows.filter((row) => row.kwh !== null)).join("rect")
      .attr("class", "bar")
      .attr("x", (row) => x(row.day)).attr("width", x.bandwidth())
      .attr("y", (row) => y(row.kwh)).attr("height", (row) => y(0) - y(row.kwh))
      .each(function (row) {
        tq.mark(this, `c|daily_energy|kwh|${row.$row}`, { chart: "daily", channel: "height" });
      });
  });
</script>
"##;

fn sql_result() -> DatasetResult {
    DatasetResult {
        dataset: "daily_energy".to_owned(),
        columns: vec![
            ResultColumn {
                name: "day".to_owned(),
                kind: ColumnKind::Time,
            },
            ResultColumn {
                name: "kwh".to_owned(),
                kind: ColumnKind::Number,
            },
        ],
        rows: vec![
            vec![json!("2026-10-20"), json!(120.5)],
            vec![json!("2026-10-21"), json!(98.25)],
            vec![json!("2026-10-22"), Value::Null],
        ],
        row_count: 3,
        meta: ResultMeta {
            queried_at: time("2026-10-26T01:02:03.456+00:00"),
            duration_ms: 84,
            source: SourceMeta::Sql {
                queryable_from: QueryablePeriod {
                    tenant_telemetry: utc("2026-07-28T00:00:00Z"),
                    tenant_telemetry_1m: utc("2026-07-28T00:00:00Z"),
                    tenant_telemetry_1h: utc("2025-10-26T00:00:00Z"),
                },
            },
        },
    }
}

fn range(interval: SemanticInterval, from: &str, to: &str) -> SemanticRange {
    SemanticRange {
        tz: "Asia/Taipei".to_owned(),
        interval,
        from: time(from),
        to: time(to),
        truncated: false,
        earliest: time("2025-10-26T08:00:00+08:00"),
        ends_after_now: true,
        resolution: SemanticResolution::Hour,
        queried_at: time("2026-10-26T09:02:03.456+08:00"),
    }
}

fn columns(fixed: &[FixedColumn]) -> Vec<ResultColumn> {
    fixed
        .iter()
        .map(|column| ResultColumn {
            name: column.name.to_owned(),
            kind: column.kind,
        })
        .collect()
}

fn semantic_result(
    dataset: &str,
    fixed: &[FixedColumn],
    rows: Vec<Vec<Value>>,
    source: SourceMeta,
) -> DatasetResult {
    DatasetResult {
        dataset: dataset.to_owned(),
        columns: columns(fixed),
        row_count: u32::try_from(rows.len()).expect("few rows"),
        rows,
        meta: ResultMeta {
            queried_at: time("2026-10-26T09:02:03.456+08:00"),
            duration_ms: 132,
            source,
        },
    }
}

fn statistics_total() -> DatasetResult {
    let range = range(
        SemanticInterval::Total,
        "2026-10-20T00:00:00+08:00",
        "2026-10-27T00:00:00+08:00",
    );
    semantic_result(
        "meter_stats",
        &STATISTICS_TOTAL_COLUMNS,
        vec![
            vec![
                json!(METER),
                json!("power"),
                json!("ok"),
                json!(10_080),
                json!(10_080),
                json!(0.4),
                json!("2026-10-21T03:00:00+08:00"),
                json!(42.75),
                json!("2026-10-23T14:00:00+08:00"),
                json!(18.125),
                json!(12.5),
                Value::Null,
                json!("2026-10-20T00:00:12+08:00"),
                json!(20.25),
                Value::Null,
                json!("2026-10-26T09:01:48+08:00"),
            ],
            vec![
                json!(LINE_B),
                json!("power"),
                json!("no_data"),
                json!(0),
                json!(0),
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
            ],
        ],
        SourceMeta::TelemetryStatistics { range },
    )
}

fn statistics_buckets() -> DatasetResult {
    let range = range(
        SemanticInterval::Hour1,
        "2026-10-26T00:00:00+08:00",
        "2026-10-27T00:00:00+08:00",
    );
    semantic_result(
        "meter_hourly",
        &STATISTICS_BUCKET_COLUMNS,
        vec![
            vec![
                json!(METER),
                json!("power"),
                json!("2026-10-26T08:00:00+08:00"),
                json!(60),
                json!(15.5),
                json!(22.0),
                json!(18.75),
                json!(19.5),
                Value::Null,
            ],
            vec![
                json!(METER),
                json!("mode"),
                json!("2026-10-26T08:00:00+08:00"),
                json!(4),
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                json!("auto"),
            ],
        ],
        SourceMeta::TelemetryStatistics { range },
    )
}

fn usage_buckets() -> DatasetResult {
    let range = range(
        SemanticInterval::Day1,
        "2026-10-19T00:00:00+08:00",
        "2026-10-26T00:00:00+08:00",
    );
    let row = |day: &str, usage: f64, readings: u32, after_gap: bool| {
        vec![
            json!(METER),
            json!("energy"),
            json!(day),
            json!(usage),
            json!(readings),
            json!(0),
            json!(after_gap),
            json!(0),
        ]
    };
    let mut result = semantic_result(
        "meter_usage",
        &USAGE_BUCKET_COLUMNS,
        vec![
            row("2026-10-19T00:00:00+08:00", 310.5, 1440, false),
            row("2026-10-20T00:00:00+08:00", 298.25, 1438, false),
            row("2026-10-22T00:00:00+08:00", 615.0, 1200, true),
        ],
        SourceMeta::TelemetryUsage { range },
    );
    if let SourceMeta::TelemetryUsage { range } = &mut result.meta.source {
        range.ends_after_now = false;
    }
    result
}

fn usage_total() -> DatasetResult {
    let range = range(
        SemanticInterval::Total,
        "2026-10-01T00:00:00+08:00",
        "2026-11-01T00:00:00+08:00",
    );
    semantic_result(
        "month_usage",
        &USAGE_TOTAL_COLUMNS,
        vec![vec![
            json!(METER),
            json!("energy"),
            json!("ok"),
            json!(7_412.75),
            json!("cumulative"),
            json!(36_000),
            json!(1),
            json!(2),
            json!(0),
            json!(true),
            json!(102_334.5),
            json!("2026-10-01T00:00:30+08:00"),
            json!(109_747.25),
            json!("2026-10-26T09:01:30+08:00"),
            json!(0),
        ]],
        SourceMeta::TelemetryUsage { range },
    )
}

fn check_request() -> CheckRequest {
    CheckRequest {
        runtime: RuntimeVersion::V1,
        page: PAGE.to_owned(),
        spec: spec().public(),
        datasets: vec![sql_result(), statistics_total(), usage_buckets()],
        locale: Locale::ZhTw,
        tz: "Asia/Taipei".to_owned(),
    }
}

fn verdict_passed() -> CheckVerdict {
    let mark = |reference: &str, value: Value| MarkReport {
        reference: reference.to_owned(),
        value,
    };
    CheckVerdict::Passed {
        report: CheckReport {
            marks: vec![
                mark("k|line_name", json!("Line 2")),
                mark("f|week_kwh", json!(1223.75)),
                mark("k|target_kwh", json!(1200.0)),
                mark("c|daily_energy|day|0", json!("2026-10-20")),
                mark("c|daily_energy|day|1", json!("2026-10-21")),
                mark("c|daily_energy|day|2", json!("2026-10-22")),
                mark("t|daily_energy|kwh|0", json!(0)),
                mark("t|daily_energy|kwh|20", json!(20)),
                mark("c|daily_energy|kwh|0", json!(120.5)),
                mark("c|daily_energy|kwh|1", json!(98.25)),
            ],
        },
    }
}

fn verdict_failed() -> CheckVerdict {
    CheckVerdict::Failed {
        problems: vec![
            Problem::LiteralNumber {
                place: LiteralPlace::Text,
                literal: "1200".to_owned(),
                tag: Some("p".to_owned()),
            },
            Problem::Mismatch {
                reference: "c|daily_energy|kwh|1".to_owned(),
                value: json!(98.25),
                tag: Some("span".to_owned()),
            },
            Problem::Unmapped {
                reference: Some("c|daily_energy|kwh|5".to_owned()),
                tag: Some("td".to_owned()),
            },
            Problem::Unmapped {
                reference: None,
                tag: Some("rect".to_owned()),
            },
            Problem::MarkGeometry {
                reference: "c|daily_energy|kwh|0".to_owned(),
                channel: GeometryChannel::Height,
            },
            Problem::ScriptError {
                message: Some(
                    "TypeError: Cannot read properties of null (reading 'kwh')".to_owned(),
                ),
            },
            Problem::NotRendered,
            Problem::Navigated,
            Problem::Timeout {
                phase: TimeoutPhase::Render,
            },
            Problem::TooLarge {
                limit: SizeLimit::Elements,
                max: 20_000,
            },
            Problem::UnsupportedContent {
                content: ContentKind::Canvas,
            },
        ],
    }
}

fn dashboard_view() -> DashboardView {
    let user =
        UserId::from_uuid(Uuid::parse_str("0190a5b2-0000-7000-8000-000000000001").expect("uuid"));
    let id = DashboardId::from_uuid(
        Uuid::parse_str("0190a5b2-0000-7000-8000-0000000000d1").expect("uuid"),
    );
    DashboardView {
        dashboard: DashboardSummary {
            dashboard_id: id,
            kind: DashboardKind::Draft,
            title: "本週用電".to_owned(),
            latest_version: 3,
            created_by: user,
            created_at: utc("2026-10-26T01:00:00Z"),
            updated_at: utc("2026-10-26T01:05:00Z"),
            archived_at: None,
            conversation_id: Some(
                Uuid::parse_str("0190a5b2-0000-7000-8000-0000000000c1").expect("uuid"),
            ),
            saved: Some(SavedBase {
                dashboard_id: DashboardId::from_uuid(
                    Uuid::parse_str("0190a5b2-0000-7000-8000-0000000000d0").expect("uuid"),
                ),
                version: 7,
            }),
        },
        version: VersionContent {
            version: 3,
            runtime: RuntimeVersion::V1,
            title: "本週用電".to_owned(),
            description: Some("Daily energy and this week's usage against the target.".to_owned()),
            spec: spec(),
            page: PAGE.to_owned(),
            origin: VersionOrigin::Agent,
            restored_from: None,
            created_by: user,
            created_at: utc("2026-10-26T01:05:00Z"),
        },
        permissions: Permissions {
            can_publish: true,
            can_restore: true,
            can_archive: false,
        },
    }
}

/// Writes or compares `value` as the fixture `name`, and checks that the fixture reads back
/// into `T` and writes out unchanged.
fn fixture<T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug>(name: &str, value: &T) {
    let json = serde_json::to_value(value).expect("serialize");
    assert_json_fixture(&format!("dashboard/{name}"), &json);
    let read: T = serde_json::from_value(json.clone()).expect("the fixture reads back");
    assert_eq!(&read, value, "{name}");
}

#[test]
fn fixtures_match_the_types() {
    let spec = spec();
    assert_eq!(
        validate_version("本週用電", None, &spec, PAGE),
        vec![],
        "the fixture spec is valid"
    );
    fixture("spec.json", &spec);
    fixture("dashboard-view.json", &dashboard_view());
    let results = [
        ("result-sql.json", sql_result(), 8),
        ("result-statistics-total.json", statistics_total(), 2),
        ("result-statistics-buckets.json", statistics_buckets(), 48),
        ("result-usage-buckets.json", usage_buckets(), 7),
        ("result-usage-total.json", usage_total(), 1),
    ];
    for (name, result, max_rows) in &results {
        assert_eq!(result.validate(*max_rows), Ok(()), "{name}");
        fixture(name, result);
    }
    let request = check_request();
    assert_eq!(request.validate(), Ok(()));
    fixture("check-request.json", &request);
    for (name, verdict) in [
        ("verdict-passed.json", verdict_passed()),
        ("verdict-failed.json", verdict_failed()),
    ] {
        assert_eq!(verdict.validate(&request), Ok(()), "{name}");
        fixture(name, &verdict);
    }
    assert_eq!(spec.check_formulas(&request.datasets), vec![]);
}

#[test]
fn the_report_values_are_the_formulas_over_the_data() {
    let request = check_request();
    let value = |formula: &str| {
        Formula::parse(formula)
            .expect("formula")
            .evaluate(request.datasets.as_slice())
    };
    assert_eq!(value("sum(meter_usage.usage)"), Some(1223.75));
    let CheckVerdict::Passed { report } = verdict_passed() else {
        unreachable!("the passed fixture")
    };
    let week = report
        .marks
        .iter()
        .find(|mark| mark.reference == "f|week_kwh")
        .expect("week mark");
    assert_eq!(week.value, json!(1223.75));
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FormulaVectors {
    #[serde(rename = "about")]
    _about: String,
    datasets: BTreeMap<String, VectorDataset>,
    cases: Vec<FormulaCase>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct VectorDataset {
    max_rows: u32,
    columns: Vec<ResultColumn>,
    rows: Vec<Vec<Value>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FormulaCase {
    formula: String,
    #[serde(default)]
    ast: Option<Value>,
    /// Present (a number, or `null` for no value) on a valid case.
    #[serde(default, deserialize_with = "present")]
    value: Option<Value>,
    #[serde(default)]
    error: Option<FormulaErrorClass>,
    #[serde(default)]
    position: Option<u32>,
}

/// A field that is present, `null` included.
fn present<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Option<Value>, D::Error> {
    Value::deserialize(deserializer).map(Some)
}

/// The vectors' datasets as a formula reads them.
struct VectorData<'a>(&'a BTreeMap<String, VectorDataset>);

impl FormulaData for VectorData<'_> {
    fn cells(&self, dataset: &str, column: &str) -> Option<Vec<Cell>> {
        let dataset = self.0.get(dataset)?;
        let index = dataset.columns.iter().position(|c| c.name == column)?;
        Some(
            dataset
                .rows
                .iter()
                .map(|row| row.get(index).map_or(Cell::Null, Cell::of_json))
                .collect(),
        )
    }
}

/// Whether `actual` and `expected` are the same tree with the same numbers. serde_json parses
/// decimal text without `float_roundtrip`, which may land one or two units in the last place
/// off the correctly rounded double (JavaScript's `JSON.parse` is exact), so numbers compare
/// within that.
fn same(actual: &Value, expected: &Value) -> bool {
    match (actual, expected) {
        (Value::Number(a), Value::Number(b)) => {
            let (a, b) = (a.as_f64().expect("finite"), b.as_f64().expect("finite"));
            a == b || (a - b).abs() <= 2.0 * f64::EPSILON * a.abs().max(b.abs())
        }
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(a, b)| same(a, b))
        }
        (a, b) => a == b,
    }
}

#[test]
fn formula_vectors() {
    let text = fs::read_to_string(fixture_path("dashboard/formula-vectors.json")).expect("read");
    let vectors: FormulaVectors = serde_json::from_str(&text).expect("vectors");
    assert!(vectors.cases.len() >= 90);
    let mut classes = std::collections::BTreeSet::new();
    for case in &vectors.cases {
        let shapes = |name: &str| {
            vectors.datasets.get(name).map(|dataset| DatasetShape {
                max_rows: dataset.max_rows,
                columns: Some(
                    dataset
                        .columns
                        .iter()
                        .map(|column| (column.name.clone(), column.kind))
                        .collect(),
                ),
            })
        };
        let outcome = Formula::parse(&case.formula)
            .and_then(|formula| formula.check(shapes).map(|()| formula));
        match (&case.error, outcome) {
            (Some(class), Err(error)) => {
                assert_eq!(error.class, *class, "{:?}", case.formula);
                assert_eq!(error.position, case.position, "{:?}", case.formula);
                classes.insert(format!("{class:?}"));
            }
            (None, Ok(formula)) => {
                let ast = case.ast.as_ref().expect("a valid case has its tree");
                let tree = formula.to_sexpr();
                assert!(same(&tree, ast), "{:?}: {tree} is not {ast}", case.formula);
                let expected = case.value.as_ref().expect("a valid case has its value");
                let actual = formula
                    .evaluate(&VectorData(&vectors.datasets))
                    .map_or(Value::Null, |number| json!(number));
                assert!(
                    same(&actual, expected),
                    "{:?}: {actual} is not {expected}",
                    case.formula
                );
            }
            (expected, actual) => {
                panic!("{:?}: expected {expected:?}, got {actual:?}", case.formula)
            }
        }
    }
    assert_eq!(
        classes.len(),
        FormulaErrorClass::ALL.len(),
        "every error class has a vector: {classes:?}"
    );
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReferenceVectors {
    #[serde(rename = "about")]
    _about: String,
    valid: Vec<String>,
    invalid: Vec<String>,
}

#[test]
fn reference_vectors() {
    let text = fs::read_to_string(fixture_path("dashboard/reference-vectors.json")).expect("read");
    let vectors: ReferenceVectors = serde_json::from_str(&text).expect("vectors");
    for text in &vectors.valid {
        let parsed: Ref = text.parse().unwrap_or_else(|_| panic!("{text:?} is valid"));
        assert_eq!(&parsed.to_string(), text);
    }
    for text in &vectors.invalid {
        assert!(text.parse::<Ref>().is_err(), "{text:?} is invalid");
    }
}

#[test]
fn fixed_columns_kinds_match_their_fixtures() {
    for (result, columns) in [
        (statistics_buckets(), &STATISTICS_BUCKET_COLUMNS[..]),
        (statistics_total(), &STATISTICS_TOTAL_COLUMNS[..]),
        (usage_buckets(), &USAGE_BUCKET_COLUMNS[..]),
        (usage_total(), &USAGE_TOTAL_COLUMNS[..]),
    ] {
        assert_eq!(result.columns.len(), columns.len());
        assert!(result.rows.iter().all(|row| row.len() == columns.len()));
    }
    assert_eq!(
        ColumnKind::from_clickhouse_type("Nullable(Float64)"),
        Some(ColumnKind::Number)
    );
}

/// The public API's schemas, as the assistant will register them in `api/openapi.json`: they
/// build, every one is named `Dashboard…` (no clash with another service's schema of the same
/// name), and the requests refuse unknown fields in the schema too.
#[test]
fn public_schemas_build_with_distinct_names() {
    use contract::api::dashboard::public::{
        ArchiveState, DashboardList, DatasetRunRequest, PublishRequest, RestoreRequest,
        VersionCreated, VersionList,
    };
    use utoipa::OpenApi;

    #[derive(OpenApi)]
    #[openapi(components(schemas(
        DashboardView,
        DashboardList,
        VersionList,
        PublishRequest,
        RestoreRequest,
        VersionCreated,
        ArchiveState,
        DatasetRunRequest,
        DatasetResult
    )))]
    struct Doc;

    let doc = serde_json::to_value(Doc::openapi()).expect("serialize");
    let schemas = doc["components"]["schemas"].as_object().expect("schemas");
    // The typed IDs are shared schemas: every service that uses one writes the same.
    let names: Vec<&str> = schemas.keys().map(String::as_str).collect();
    for name in &names {
        assert!(
            name.starts_with("Dashboard") || ["DeviceId", "UserId"].contains(name),
            "{name} in {names:?}"
        );
    }
    for request in [
        "DashboardPublishRequest",
        "DashboardRestoreRequest",
        "DashboardDatasetRunRequest",
    ] {
        assert_eq!(
            schemas[request]["additionalProperties"],
            json!(false),
            "{request}"
        );
    }
}
