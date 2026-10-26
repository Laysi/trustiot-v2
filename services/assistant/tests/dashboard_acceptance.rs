//! Acceptance tests of the AI dashboards (Laysi/trustiot-v2#150) that span services. The main
//! agent asks with a scripted model (the AI gateway stand-in); its `dashboard` tool runs the
//! datasets as the asking user and checks every write with the dashboard check service, run
//! from its image as the chart runs the pod (read-only root, an in-memory /tmp of the chart's
//! size, every capability dropped, no privilege escalation, the engine's default seccomp
//! profile, the chart's memory limit, user 65532), behind a loopback proxy that keeps each
//! verdict as the service sent it:
//!
//! - criterion 2, a page that does not match its data or marks something that is not in it is
//!   refused, and the user still sees the previous version: with the sandbox runtime's fixtures
//!   (frontend/sandbox/fixtures) and telemetry-query's stand-in answering their data, the model
//!   writes the good page (version 1), then the page with one bar at half its height (refused,
//!   `mark_geometry`), then the page with marks on a row and a dataset that do not exist and a
//!   shape that is no mark (refused, `unmapped`). Each verdict meets its case in cases.json, the
//!   model gets the problems, and the public API still shows version 1 with the good page.
//! - criterion 1, the numbers on the dashboard equal ClickHouse's: the synthetic lab dataset
//!   (datasets/lab/synthetic) is loaded into a throwaway ClickHouse with the product's schema,
//!   and telemetry-query's routes, served in this process on the product's schemas, run the
//!   datasets. Every value in the checker's report equals the same quantity computed by a
//!   direct query of `trustiot.telemetry` with a test account that reads every row; the test
//!   `the_numbers_on_a_dashboard_equal_clickhouses` lists the quantities.
//!
//! Criterion 4 (no SQL or HTML in the answer) is tests/answer_guard.rs.
//!
//! The tests need the image, so they are ignored by default. `bun test
//! deploy/images/dashboard-check` builds `localhost/trustiot-e2e/dashboard-check:image-test`;
//! then `DASHBOARD_CHECK_IMAGE=<image> cargo test -p assistant --test dashboard_acceptance --
//! --ignored` runs them. They start containers with docker or podman (the check service,
//! Postgres, ClickHouse); without the image or a container runtime they skip, and with `CI` set
//! they fail instead. They run one at a time ([`SERIAL`]), so a small machine holds them.

#![allow(
    clippy::print_stderr,
    reason = "prints every compared value: the evidence `-- --nocapture` shows"
)]

/// The ClickHouse container and its HTTP client, shared with the ingest service's schema tests.
#[path = "../../ingest/tests/common/mod.rs"]
mod clickhouse;
mod dashboard_world;
mod support;

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use assistant::dashboards::caps::RunCaps;
use assistant::dashboards::checker::CHECK_CALL_TIMEOUT;
use assistant::tools::Tool;
use assistant::tools::dashboard::DATASETS_TIMEOUT;
use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::header::CONTENT_TYPE;
use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use chrono::{DateTime, DurationRound, FixedOffset, TimeDelta, Utc};
use contract::api::dashboard::check::CHECK_ROUTE;
use contract::api::telemetry_query::{QueryableFrom, RunSql, SqlColumn, SqlResult};
use contract::{ServiceName, TenantId, TenantRole};
use dashboard_world::{Peers, Person, Question, SQL_PATH, World, call, dashboard_tool, world};
use lab_dataset::{Dataset, Target};
use serde_json::{Value, json};
use service_kit::tenant::TenantDb;
use sqlx::postgres::PgPoolOptions;
use support::{Recorded, StandIn, agent, fixture, ok, respond, settings, step, tool_results};
use telemetry_query::semantic::range::Resolution;
use telemetry_query::semantic::{
    CumulativeCheck, QueryRange, SeriesQuery, SeriesUsage, Usage, UsageRow, UsageStatus, Verdict,
};
use testkit::product::{DATABASE, product_postgres};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use uuid::Uuid;

use self::clickhouse::{Account, ClickHouse, unavailable};

/// One test at a time: each starts the check service, and criterion 1 also ClickHouse.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Names the dashboard check image the tests run.
const IMAGE_ENV: &str = "DASHBOARD_CHECK_IMAGE";

/// The service's numeric user in the image (deploy/images/dashboard-check/Containerfile).
const CHECK_USER: &str = "65532:65532";

/// The longest the check service may take to become ready: a bound only, far above what it
/// takes, so a loaded machine does not fail the test (CLAUDE.md, 「Test time limits」).
const START_LIMIT: Duration = Duration::from_secs(120);

/// The sandbox runtime's fixtures (frontend/sandbox/README.md, 「固定樣本」).
const FIXTURES: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../frontend/sandbox/fixtures"
);

/// The chart's values, for the dashboard check's limits.
const CHART_VALUES: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../deploy/chart/values.yaml"
);

// ---------------------------------------------------------------------------------------------
// The check service in its image, behind a recording proxy.

/// `docker`, or `podman` when docker is not installed (deploy/images/dashboard-check/README.md).
fn engine() -> Option<&'static str> {
    ["docker", "podman"].into_iter().find(|engine| {
        Command::new(engine)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    })
}

/// Runs `engine args` and returns its output; panics when it fails.
fn engine_output(engine: &str, args: &[&str]) -> String {
    let output = Command::new(engine)
        .args(args)
        .output()
        .unwrap_or_else(|error| panic!("{engine} {}: {error}", args.join(" ")));
    assert!(
        output.status.success(),
        "{engine} {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// A size of the chart (`512Mi`) as the engine takes it (`512m`).
fn engine_size(size: &str) -> String {
    let mebibytes = size
        .strip_suffix("Mi")
        .filter(|digits| !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()))
        .unwrap_or_else(|| panic!("the chart's size {size:?} is not in Mi"));
    format!("{mebibytes}m")
}

/// The dashboard check's memory limit and /tmp size in deploy/chart/values.yaml, read line by
/// line from its block under `services` (the keys are plain scalars; no YAML parser needed).
fn chart_limits() -> (String, String) {
    let text = std::fs::read_to_string(CHART_VALUES).expect("read deploy/chart/values.yaml");
    let block: Vec<&str> = text
        .lines()
        .skip_while(|line| *line != "  dashboard-check:")
        .skip(1)
        .take_while(|line| line.is_empty() || line.starts_with("    "))
        .map(str::trim)
        .collect();
    let value = |lines: &[&str], key: &str| -> Option<String> {
        lines
            .iter()
            .find_map(|line| line.strip_prefix(key))
            .map(|value| value.trim().trim_matches('"').to_owned())
    };
    let limits = block
        .iter()
        .position(|line| *line == "limits:")
        .expect("services.dashboard-check.resources.limits in the chart");
    let memory = value(block.get(limits..).unwrap_or_default(), "memory:")
        .expect("the dashboard check's memory limit in the chart");
    let tmp = value(&block, "tmpSizeLimit:").expect("the dashboard check's tmpSizeLimit");
    (engine_size(&memory), engine_size(&tmp))
}

/// A container that is removed with its volumes when dropped.
struct Container {
    engine: &'static str,
    name: String,
}

impl Drop for Container {
    fn drop(&mut self) {
        let _ = Command::new(self.engine)
            .args(["rm", "--force", "--volumes", &self.name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

impl Container {
    /// The loopback origin the engine published the container's `port` on.
    fn origin(&self, port: u16) -> String {
        let published = engine_output(self.engine, &["port", &self.name, &format!("{port}/tcp")]);
        let first = published.lines().next().unwrap_or_default().trim();
        format!("http://{}", first.replace("0.0.0.0:", "127.0.0.1:"))
    }
}

/// What the proxy shares: where the service is, and every answer it gave.
struct ProxyState {
    client: reqwest::Client,
    target: String,
    verdicts: Mutex<Vec<(u16, Value)>>,
}

/// The dashboard check service in a container of [`IMAGE_ENV`]'s image, and the proxy the tool
/// calls instead of it.
struct CheckService {
    /// The proxy's origin: the tool's `dashboard_check.url`.
    url: String,
    state: Arc<ProxyState>,
    proxy: JoinHandle<()>,
    _container: Container,
}

impl Drop for CheckService {
    fn drop(&mut self) {
        self.proxy.abort();
    }
}

static CONTAINERS: AtomicUsize = AtomicUsize::new(0);

impl CheckService {
    /// Starts the service and waits until it is ready; `None` (the test skips) without the image
    /// or a container runtime, unless `CI` is set.
    async fn start() -> Option<Self> {
        let Some(image) = std::env::var(IMAGE_ENV)
            .ok()
            .filter(|image| !image.is_empty())
        else {
            unavailable(&format!("{IMAGE_ENV} (the dashboard check image)"));
            return None;
        };
        let Some(engine) = engine() else {
            unavailable("docker or podman");
            return None;
        };
        let present = Command::new(engine)
            .args(["image", "inspect", &image])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        if !present {
            unavailable(&format!("the image {image} ({IMAGE_ENV})"));
            return None;
        }
        let (memory, tmp) = chart_limits();
        let container = Container {
            engine,
            name: format!(
                "trustiot-assistant-acceptance-{}-{}",
                std::process::id(),
                CONTAINERS.fetch_add(1, Ordering::SeqCst)
            ),
        };
        let tmpfs = format!("/tmp:rw,mode=1777,size={tmp}");
        engine_output(
            engine,
            &[
                "run",
                "--detach",
                "--name",
                &container.name,
                "--platform",
                "linux/arm64",
                "--user",
                CHECK_USER,
                "--read-only",
                "--tmpfs",
                &tmpfs,
                "--cap-drop",
                "ALL",
                "--security-opt",
                "no-new-privileges",
                "--memory",
                &memory,
                "--publish",
                "127.0.0.1::8080",
                "--publish",
                "127.0.0.1::8081",
                &image,
            ],
        );
        let api = container.origin(8080);
        let health = container.origin(8081);
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("an HTTP client");
        let started = Instant::now();
        loop {
            let ready = client
                .get(format!("{health}/readyz"))
                .send()
                .await
                .is_ok_and(|answer| answer.status().as_u16() == 200);
            if ready {
                break;
            }
            if started.elapsed() > START_LIMIT {
                let logs = Command::new(engine)
                    .args(["logs", &container.name])
                    .output()
                    .map(|output| {
                        format!(
                            "{}{}",
                            String::from_utf8_lossy(&output.stdout),
                            String::from_utf8_lossy(&output.stderr)
                        )
                    })
                    .unwrap_or_default();
                panic!("the dashboard check service was not ready within {START_LIMIT:?}:\n{logs}");
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }

        let state = Arc::new(ProxyState {
            // Longer than the tool waits for a check, so the tool's own timeout decides.
            client: reqwest::Client::builder()
                .timeout(CHECK_CALL_TIMEOUT + Duration::from_secs(30))
                .build()
                .expect("an HTTP client"),
            target: format!("{api}{CHECK_ROUTE}"),
            verdicts: Mutex::new(Vec::new()),
        });
        let router = Router::new()
            .route(CHECK_ROUTE, post(forward))
            .with_state(Arc::clone(&state));
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("local address");
        let proxy = tokio::spawn(async move {
            axum::serve(listener, router)
                .await
                .expect("serve the proxy");
        });
        Some(Self {
            url: format!("http://{address}"),
            state,
            proxy,
            _container: container,
        })
    }

    /// Every answer of the service so far: status and JSON body.
    fn verdicts(&self) -> Vec<(u16, Value)> {
        self.state
            .verdicts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// Sends a check to the service and keeps its answer.
async fn forward(State(state): State<Arc<ProxyState>>, body: Bytes) -> Response {
    let answer = state
        .client
        .post(&state.target)
        .header(CONTENT_TYPE, "application/json")
        .body(body)
        .send()
        .await;
    let Ok(answer) = answer else {
        return StatusCode::BAD_GATEWAY.into_response();
    };
    let status = answer.status().as_u16();
    let bytes = answer.bytes().await.unwrap_or_default();
    state
        .verdicts
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push((
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        ));
    (
        StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
        [(CONTENT_TYPE, "application/json")],
        bytes,
    )
        .into_response()
}

// ---------------------------------------------------------------------------------------------
// Shared steps.

/// Whether two JSON values are equal, numbers by value (`615` equals `615.0`).
fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(a), Value::Number(b)) => a.as_f64() == b.as_f64(),
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(a, b)| same(a, b))
        }
        (Value::Object(a), Value::Object(b)) => {
            a.len() == b.len()
                && a.iter()
                    .all(|(key, value)| b.get(key).is_some_and(|other| same(value, other)))
        }
        _ => a == b,
    }
}

/// Why `verdict` does not meet `expectation` (a case's `expect` in cases.json), with the rules
/// of frontend/sandbox/fixtures/expect.ts: a passing case needs exactly its marks, in order; a
/// failing case needs every expected problem (at least the expected fields, equal) and no
/// problem of a kind that is neither expected nor in `may_also`.
fn unmet(verdict: &Value, expectation: &Value) -> Option<String> {
    if verdict["verdict"] != expectation["verdict"] {
        return Some(format!(
            "verdict {} is not {}",
            verdict["verdict"], expectation["verdict"]
        ));
    }
    if expectation["verdict"] == "passed" {
        return (!same(&verdict["report"]["marks"], &expectation["marks"]))
            .then(|| "the report's marks are not the case's".to_owned());
    }
    let problems = verdict["problems"].as_array().cloned().unwrap_or_default();
    let expected = expectation["problems"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    for wanted in &expected {
        let fields = wanted.as_object().cloned().unwrap_or_default();
        let found = problems.iter().any(|problem| {
            fields
                .iter()
                .all(|(field, value)| same(&problem[field.as_str()], value))
        });
        if !found {
            return Some(format!("no problem {wanted}"));
        }
    }
    let allowed: BTreeSet<String> = expected
        .iter()
        .map(|problem| problem["kind"].clone())
        .chain(
            expectation["may_also"]
                .as_array()
                .cloned()
                .unwrap_or_default(),
        )
        .filter_map(|kind| kind.as_str().map(str::to_owned))
        .collect();
    problems
        .iter()
        .find(|problem| {
            !problem["kind"]
                .as_str()
                .is_some_and(|kind| allowed.contains(kind))
        })
        .map(|problem| format!("unexpected problem {problem}"))
}

/// The JSON of one tool result's text.
fn result_json(request: &Recorded) -> Value {
    let results = tool_results(request);
    let result = results.first().expect("a tool result");
    serde_json::from_str(&result.text)
        .unwrap_or_else(|_| panic!("the tool result is JSON: {}", result.text))
}

/// The JSON of the first tool result in the conversation `request` sends: the first write's.
fn first_saved(request: &Recorded) -> Option<Value> {
    let body = request.json();
    let text = body["messages"]
        .as_array()?
        .iter()
        .filter_map(|message| message["content"].as_array())
        .flatten()
        .find(|block| block["type"] == "tool_result")?["content"]
        .as_array()?
        .iter()
        .find_map(|part| part["text"].as_str())?
        .to_owned();
    serde_json::from_str(&text).ok()
}

/// Starts the agent with `tools` and the scripted model, asks `question` as `owner` in a new
/// conversation (stored as the service stores one), and returns the model's requests.
async fn ask(
    world: &World,
    owner: &Person,
    tool: Arc<dyn Tool>,
    model: support::Responder,
    question: &str,
) -> (Vec<Recorded>, String) {
    let stand_in = StandIn::start(model).await;
    let agent = agent(&stand_in, vec![tool], settings());
    let mut conversation = agent.start_conversation(owner.principal(), "");
    world.store_conversation(owner, conversation.id()).await;
    let answer = agent
        .ask(
            &mut conversation,
            question,
            Arc::new(Question::of(world, owner)),
        )
        .await
        .expect("an answer");
    (stand_in.requests(), answer.text)
}

// ---------------------------------------------------------------------------------------------
// Criterion 2: the sandbox runtime's fixtures.

/// The fixtures: the check request the pages were written for, and the cases.
struct Fixtures {
    request: Value,
    cases: Vec<Value>,
}

impl Fixtures {
    fn read() -> Self {
        let read = |name: &str| -> Value {
            let path = PathBuf::from(FIXTURES).join(name);
            let text = std::fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
            serde_json::from_str(&text).unwrap_or_else(|error| panic!("{name}: {error}"))
        };
        let cases = read("cases.json");
        assert_eq!(cases["request"], "request.json", "the cases' request");
        Self {
            request: read("request.json"),
            cases: cases["cases"].as_array().cloned().expect("the cases"),
        }
    }

    /// The case named `name`.
    fn case(&self, name: &str) -> &Value {
        self.cases
            .iter()
            .find(|case| case["name"] == name)
            .unwrap_or_else(|| panic!("the case {name}"))
    }

    /// The page of the case named `name`.
    fn page(&self, name: &str) -> String {
        let page = self.case(name)["page"].as_str().expect("the case's page");
        let path = PathBuf::from(FIXTURES).join(page);
        std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
    }

    /// The fixture's result of `dataset`.
    fn result(&self, dataset: &str) -> &Value {
        self.request["datasets"]
            .as_array()
            .and_then(|results| results.iter().find(|result| result["dataset"] == dataset))
            .unwrap_or_else(|| panic!("the fixture's result of {dataset}"))
    }

    /// The fixture's public dataset `name`: its title, description and `max_rows`.
    fn dataset(&self, name: &str) -> &Value {
        self.request["spec"]["datasets"]
            .as_array()
            .and_then(|datasets| datasets.iter().find(|dataset| dataset["name"] == name))
            .unwrap_or_else(|| panic!("the fixture's dataset {name}"))
    }

    /// The model's first write: a new draft with the fixture's datasets, computations and
    /// constants, and the good page. The SQL is what a model writes for these columns;
    /// telemetry-query's stand-in answers with the fixture's rows.
    fn first_write(&self) -> Value {
        let usage = &self.result("meter_usage")["rows"][0];
        let sources = [
            (
                "daily_energy",
                json!({"kind": "sql", "sql": "SELECT toDate(bucket, 'Asia/Taipei') AS day, \
                    sum(value_sum) AS kwh FROM tenant_telemetry_1h WHERE point = 'energy' \
                    GROUP BY day ORDER BY day"}),
            ),
            (
                "hourly_power",
                json!({"kind": "sql", "sql": "SELECT toStartOfHour(bucket, 'Asia/Taipei') AS hour, \
                    sum(value_sum) / sum(numeric_readings) AS kw FROM tenant_telemetry_1m \
                    WHERE point = 'power' GROUP BY hour ORDER BY hour"}),
            ),
            (
                "meter_usage",
                json!({"kind": "telemetry_usage", "request": {
                    "series": [{"device_id": usage[0], "point": usage[1]}],
                    "tz": "Asia/Taipei",
                    "period": "this_week",
                    "interval": "1d",
                }}),
            ),
        ];
        let datasets: Vec<Value> = sources
            .into_iter()
            .map(|(name, source)| {
                let public = self.dataset(name);
                json!({
                    "name": name,
                    "title": public["title"],
                    "description": public["description"],
                    "max_rows": public["max_rows"],
                    "source": source,
                })
            })
            .collect();
        json!({
            "action": "write",
            "title": "每日用電",
            "datasets": {"upsert": datasets},
            "computations": {"upsert": self.request["spec"]["computations"]},
            "constants": {"upsert": self.request["spec"]["constants"]},
            "page": self.page("correct"),
            "locale": self.request["locale"],
            "tz": self.request["tz"],
        })
    }

    /// The fixture's SQL result of `dataset` as telemetry-query answers it.
    fn sql_answer(&self, dataset: &str, types: [&str; 2], sql: String) -> Value {
        let result = self.result(dataset);
        let names = result["columns"]
            .as_array()
            .expect("columns")
            .iter()
            .map(|column| column["name"].as_str().expect("a name").to_owned());
        let answer = SqlResult {
            sql,
            columns: names
                .zip(types)
                .map(|(name, data_type)| SqlColumn {
                    name,
                    data_type: data_type.to_owned(),
                })
                .collect(),
            rows: serde_json::from_value(result["rows"].clone()).expect("rows"),
            queryable_from: serde_json::from_value::<QueryableFrom>(
                result["meta"]["source"]["queryable_from"].clone(),
            )
            .expect("queryable_from"),
            queried_at: serde_json::from_value(result["meta"]["queried_at"].clone())
                .expect("queried_at"),
        };
        serde_json::to_value(answer).expect("JSON")
    }

    /// The fixture's usage result as telemetry-query answers `query`.
    fn usage_answer(&self, query: &SeriesQuery) -> Value {
        let result = self.result("meter_usage");
        let range = &result["meta"]["source"]["range"];
        let time = |value: &Value| -> DateTime<FixedOffset> {
            serde_json::from_value(value.clone()).expect("a time")
        };
        let rows: Vec<UsageRow> = result["rows"]
            .as_array()
            .expect("rows")
            .iter()
            .map(|row| UsageRow {
                time: time(&row[2]),
                usage: row[3].as_f64().expect("usage"),
                readings: row[4].as_u64().expect("readings"),
                drops: row[5].as_u64().expect("drops"),
                after_gap: row[6].as_bool().expect("after_gap"),
                suspicious_steps: row[7].as_u64().expect("suspicious_steps"),
            })
            .collect();
        let key = query.series.first().expect("one series");
        let series = SeriesUsage {
            device_id: key.device_id,
            point: key.point.clone(),
            status: UsageStatus::Ok,
            check: Some(CumulativeCheck {
                verdict: Verdict::Cumulative,
                drops: 0,
                rises: 47,
                comparisons: 47,
                from: time(&range["from"]),
                to: time(&range["to"]),
            }),
            total: Some(rows.iter().map(|row| row.usage).sum()),
            drops: 0,
            readings: rows.iter().map(|row| row.readings).sum(),
            first_reading: None,
            last_reading: None,
            baseline: true,
            gaps: 1,
            longest_gap: None,
            suspicious_steps: 0,
            largest_suspicious_step: None,
            skipped_cells: 0,
            highest: None,
            lowest: None,
            rows,
        };
        let answer = Usage {
            range: QueryRange {
                tz: query.tz.clone(),
                interval: query.interval,
                from: time(&range["from"]),
                to: time(&range["to"]),
                truncated: false,
                earliest: time(&range["earliest"]),
                ends_after_now: false,
                resolution: Resolution::Hour,
                queried_at: time(&range["queried_at"]),
            },
            series: vec![series],
        };
        serde_json::to_value(answer).expect("JSON")
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dashboard check image: DASHBOARD_CHECK_IMAGE=<image> cargo test -p assistant --test dashboard_acceptance -- --ignored"]
async fn a_page_that_does_not_match_its_data_is_refused_and_the_user_keeps_the_previous_version() {
    let _serial = SERIAL.lock().await;
    let Some(check) = CheckService::start().await else {
        return;
    };
    let Some(world) = world().await else {
        return;
    };
    let fixtures = Arc::new(Fixtures::read());
    let peers = Peers::start(Arc::clone(&world.issuer)).await;
    let answers = Arc::clone(&fixtures);
    peers.script(Arc::new(move |call| {
        let answers = Arc::clone(&answers);
        Box::pin(async move {
            if call.path == SQL_PATH {
                let sql: RunSql = serde_json::from_value(call.body.clone()).ok()?;
                let (dataset, types) = if sql.sql.contains("AS kwh") {
                    ("daily_energy", ["Date", "Nullable(Float64)"])
                } else {
                    ("hourly_power", ["DateTime('Asia/Taipei')", "Float64"])
                };
                return Some((200, answers.sql_answer(dataset, types, sql.sql)));
            }
            let query: SeriesQuery = serde_json::from_value(call.body.clone()).ok()?;
            Some((200, answers.usage_answer(&query)))
        })
    }));
    let tool = dashboard_tool(
        &world,
        &peers.url,
        &check.url,
        CHECK_CALL_TIMEOUT,
        DATASETS_TIMEOUT,
    );
    let owner = world.person(TenantId::generate(), TenantRole::Editor);

    // The model writes the good page, then (with the draft's ID from the tool's result) a page
    // with a bar at half its height, then one that marks what is not in the data, then answers.
    let first = fixtures.first_write();
    let pages = [
        fixtures.page("wrong-bar-height"),
        fixtures.page("unmapped-mark"),
    ];
    let model = respond(move |request, index| {
        let input = match index {
            0 => Some(first.clone()),
            1 | 2 => first_saved(request)
                .and_then(|saved| saved["dashboard_id"].as_str().map(str::to_owned))
                .map(|id| {
                    json!({
                        "action": "write",
                        "dashboard_id": id,
                        "base_version": 1,
                        "page": pages[index - 1],
                    })
                }),
            _ => None,
        };
        // Without a draft to edit (the first write was not saved), the model answers; the
        // assertions below then report the first result.
        match input {
            Some(input) => ok(step(index, &[("dashboard", input)])),
            None => ok(fixture("answer_end_turn.json")),
        }
    });
    let (requests, answer) = ask(
        &world,
        &owner,
        tool,
        model,
        "Show this week's energy on a dashboard.",
    )
    .await;
    assert!(!answer.is_empty(), "the model's answer reaches the user");
    assert_eq!(requests.len(), 4, "three writes, then the answer");

    // 1. The good page saved version 1 of a new draft.
    let saved = result_json(&requests[1]);
    assert_eq!(saved["saved"], true, "{saved}");
    assert_eq!(saved["version"], 1);
    assert_eq!(saved["kind"], "draft");
    let id = saved["dashboard_id"]
        .as_str()
        .expect("the draft's ID")
        .to_owned();

    // 2 and 3. Each bad page is refused with the check's problems, and nothing is saved.
    let geometry = result_json(&requests[2]);
    assert_eq!(geometry["saved"], false, "{geometry}");
    assert!(tool_results(&requests[2])[0].is_error);
    let unmapped = result_json(&requests[3]);
    assert_eq!(unmapped["saved"], false, "{unmapped}");
    let kinds = |result: &Value| -> BTreeSet<String> {
        result["problems"]
            .as_array()
            .expect("problems")
            .iter()
            .map(|problem| problem["kind"].as_str().expect("a kind").to_owned())
            .collect()
    };
    assert_eq!(
        kinds(&geometry),
        BTreeSet::from(["mark_geometry".to_owned()])
    );
    assert_eq!(kinds(&unmapped), BTreeSet::from(["unmapped".to_owned()]));
    let bar = geometry["problems"]
        .as_array()
        .and_then(|problems| problems.first())
        .expect("the bar's problem");
    assert_eq!(bar["reference"], "c|daily_energy|kwh|2");
    assert_eq!(bar["channel"], "height");
    assert!(bar["hint"].as_str().is_some_and(|hint| !hint.is_empty()));

    // The service's verdicts, as it sent them, meet the fixture cases.
    let verdicts = check.verdicts();
    assert_eq!(verdicts.len(), 3, "one check per write: {verdicts:?}");
    for ((status, verdict), case) in
        verdicts
            .iter()
            .zip(["correct", "wrong-bar-height", "unmapped-mark"])
    {
        assert_eq!(*status, 200, "{case}: {verdict}");
        if let Some(why) = unmet(verdict, &fixtures.case(case)["expect"]) {
            panic!("{case}: {why}\nverdict: {verdict}");
        }
        eprintln!("{case}: {verdict}");
    }
    // The tool shows the marked values from its own data, without the axis ticks.
    let marked: Vec<Value> = verdicts[0].1["report"]["marks"]
        .as_array()
        .expect("the report's marks")
        .iter()
        .filter(|mark| {
            !mark["reference"]
                .as_str()
                .is_some_and(|reference| reference.starts_with("t|"))
        })
        .cloned()
        .collect();
    assert!(!marked.is_empty(), "the page marks values");
    assert_eq!(
        saved["check"]["marks"],
        marked.len(),
        "the tool reports the check's marks without the ticks"
    );
    let values = saved["check"]["values"].as_array().expect("the values");
    assert!(!values.is_empty() && values.len() <= marked.len());
    for (shown, mark) in values.iter().zip(&marked) {
        assert_eq!(shown["reference"], mark["reference"]);
        // The same value: the tool's from its data, the check's as the runtime resolved it
        // (numbers may differ in the last binary places).
        match (shown["value"].as_f64(), mark["value"].as_f64()) {
            (Some(own), Some(checked)) => assert!(
                (own - checked).abs() <= 1e-9 * own.abs().max(1.0),
                "{shown} {mark}"
            ),
            _ => assert_eq!(shown["value"], mark["value"]),
        }
    }

    // The datasets ran once in the question: the edits reused their results.
    assert_eq!(peers.calls().len(), 3, "{:?}", peers.calls());
    assert!(peers.unexpected().is_empty(), "{:?}", peers.unexpected());

    // The user still sees version 1 with the good page, and no other version exists.
    let api = world.api(&peers, RunCaps::default()).await;
    let token = world.token(&owner);
    let shown = call(
        &api.url,
        Method::GET,
        &format!("/api/assistant/v1/dashboards/{id}"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(shown.status, 200, "{}", shown.text);
    let shown = shown.json();
    assert_eq!(shown["dashboard"]["latest_version"], 1);
    assert_eq!(shown["version"]["version"], 1);
    assert_eq!(shown["version"]["page"], fixtures.page("correct"));
    let versions = call(
        &api.url,
        Method::GET,
        &format!("/api/assistant/v1/dashboards/{id}/versions"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(versions.status, 200, "{}", versions.text);
    let numbers: Vec<Value> = versions.json()["versions"]
        .as_array()
        .expect("versions")
        .iter()
        .map(|version| version["version"].clone())
        .collect();
    assert_eq!(numbers, [json!(1)]);
}

// ---------------------------------------------------------------------------------------------
// Criterion 1: the synthetic lab dataset in ClickHouse.

/// The service's ClickHouse account (`0008_telemetry_query.sql`).
const SERVICE_ACCOUNT: &str = "trustiot_telemetry_query";

/// Sets the service account's password and lets it log in, as the migration Job does with the
/// password it generates into the service's Secret.
const SET_SERVICE_PASSWORD: &str = "ALTER USER trustiot_telemetry_query
IDENTIFIED WITH sha256_password BY {password:String} HOST ANY";

/// An account that reads every row of the telemetry table, for the expected values. Test-only:
/// the product has no such reader.
const CREATE_REFERENCE: &str = "CREATE USER IF NOT EXISTS trustiot_test_reference
IDENTIFIED WITH sha256_password BY {password:String} HOST ANY";
const GRANT_REFERENCE: &str = "GRANT SELECT ON trustiot.telemetry TO trustiot_test_reference";
const REFERENCE_POLICY: &str = "CREATE ROW POLICY IF NOT EXISTS test_reference_all_rows
ON trustiot.telemetry AS PERMISSIVE FOR SELECT USING 1 TO trustiot_test_reference";

/// The readiness row of a tenant's account, which telemetry-query's consumer writes once the
/// account exists (here the test creates the account itself).
const ACCOUNT_READY: &str =
    "INSERT INTO telemetry_query.tenant_accounts (tenant_id) VALUES ($1::uuid)";

/// A random password of 64 hex digits, made at run time.
fn random_password() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

/// ClickHouse with the product's schema and a tenant's account, and telemetry-query's routes in
/// this process.
struct Telemetry {
    clickhouse: ClickHouse,
    reference: Account,
    url: String,
    server: JoinHandle<()>,
}

impl Drop for Telemetry {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl Telemetry {
    /// Starts ClickHouse, applies the product's ClickHouse migrations, creates `tenant`'s account
    /// as the service does, and serves telemetry-query's routes; `None` without a container
    /// runtime, unless `CI` is set.
    async fn start(world: &World, tenant: TenantId) -> Option<Self> {
        let product = product_postgres(&["telemetry_query_app"]).await?;
        let clickhouse = ClickHouse::start_throwaway().await?;
        for migration in migrate::registry::PRODUCT.clickhouse {
            let statements = migrate::clickhouse::split_statements(migration.sql)
                .unwrap_or_else(|error| panic!("{}: {error}", migration.name));
            for statement in statements {
                clickhouse.admin(&statement).await.unwrap_or_else(|error| {
                    panic!("{}: {statement}\n{}", migration.name, error.body)
                });
            }
        }
        let service_password = random_password();
        clickhouse
            .query_with(
                &clickhouse.admin,
                SET_SERVICE_PASSWORD,
                &[("param_password", &service_password)],
            )
            .await
            .unwrap_or_else(|error| panic!("set the service's password: {}", error.body));
        let reference = Account {
            user: "trustiot_test_reference".to_owned(),
            password: random_password(),
        };
        clickhouse
            .query_with(
                &clickhouse.admin,
                CREATE_REFERENCE,
                &[("param_password", &reference.password)],
            )
            .await
            .unwrap_or_else(|error| panic!("create the reference account: {}", error.body));
        for statement in [GRANT_REFERENCE, REFERENCE_POLICY] {
            clickhouse.admin_ok(statement).await;
        }

        let accounts = telemetry_query::accounts::Accounts::new(
            telemetry_query::clickhouse::ClickHouse::new(&clickhouse.url, None)
                .expect("a ClickHouse client"),
            SERVICE_ACCOUNT,
            &service_password,
        );
        accounts
            .ensure(tenant)
            .await
            .expect("create the tenant's account");
        let mut admin = product.superuser().await;
        sqlx::query(ACCOUNT_READY)
            .bind(tenant.to_string())
            .execute(&mut admin)
            .await
            .expect("record the account as ready");

        let db = TenantDb::connect_lazy(
            product.postgres.options("telemetry_query_app", DATABASE),
            PgPoolOptions::new().max_connections(4),
        )
        .expect("the service role's pool verifies the server");
        let verifier = world.issuer.verifier(ServiceName::TelemetryQuery);
        let semantic = telemetry_query::semantic::routes(telemetry_query::history::Routes::new(
            verifier.clone(),
            db.clone(),
            accounts.clone(),
        ));
        let router = telemetry_query::history::routes(verifier, db, accounts).merge(semantic);
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("local address");
        let server = tokio::spawn(async move {
            axum::serve(listener, router)
                .await
                .expect("serve telemetry-query");
        });
        Some(Self {
            clickhouse,
            reference,
            url: format!("http://{address}"),
            server,
        })
    }

    /// The rows of `sql` (which ends in `FORMAT JSONEachRow`) as the reference account, with
    /// `params`.
    async fn reference_rows(&self, sql: &str, params: &[(&str, &str)]) -> Vec<Value> {
        let text = self
            .clickhouse
            .query_with(&self.reference, sql, params)
            .await
            .unwrap_or_else(|error| panic!("{sql}\n{}", error.body));
        text.lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).expect("a JSON row"))
            .collect()
    }
}

/// The power readings of each device: count, extremes, sum, mean and the latest, ordered as a
/// UUID orders (as the dataset's `ORDER BY device_id` does).
const REFERENCE_POWER: &str = "SELECT device_id, count() AS readings, min(value_float) AS low,
  max(value_float) AS high, sum(value_float) AS total, avg(value_float) AS mean,
  argMax(value_float, ts) AS last
FROM trustiot.telemetry
WHERE tenant_id = {tenant:UUID} AND point = 'power_kw'
GROUP BY device_id ORDER BY device_id
FORMAT JSONEachRow";

/// One meter's power per UTC hour: the highest reading and the mean, from the readings rather
/// than the hourly cells.
const REFERENCE_HOURLY: &str =
    "SELECT toStartOfHour(ts, 'UTC') AS hour, max(value_float) AS peak_kw,
  avg(value_float) AS mean_kw
FROM trustiot.telemetry
WHERE tenant_id = {tenant:UUID} AND device_id = {device:UUID} AND point = 'power_kw'
GROUP BY hour ORDER BY hour
FORMAT JSONEachRow";

/// Every power reading together: the highest, the mean, and the spread from the lowest.
const REFERENCE_POWER_ALL: &str = "SELECT max(value_float) AS peak, avg(value_float) AS mean,
  max(value_float) - min(value_float) AS spread
FROM trustiot.telemetry
WHERE tenant_id = {tenant:UUID} AND point = 'power_kw'
FORMAT JSONEachRow";

/// The usage of the two counters from consecutive readings (services/telemetry-query/README.md,
/// 「用量」): the increase from the previous reading, or the reading itself after a drop (a
/// reset or a wrap starts again from 0); the first reading of a range without an earlier one
/// adds nothing. Also the readings, and the usage per reading interval.
const REFERENCE_USAGE: &str = "SELECT device_id, point,
  sum(if(previous IS NULL, 0, if(value >= previous, value - previous, value))) AS total,
  count() AS readings,
  total / (readings - 1) AS per_interval
FROM (
  SELECT device_id, point, ts, value,
    lagInFrame(toNullable(value)) OVER (PARTITION BY device_id, point ORDER BY ts
      ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) AS previous
  FROM (
    SELECT device_id, point, ts, if(value_type = 'int', toFloat64(value_int), value_float) AS value
    FROM trustiot.telemetry
    WHERE tenant_id = {tenant:UUID}
      AND ((device_id = {meter_1:UUID} AND point = 'energy_kwh')
        OR (device_id = {meter_2:UUID} AND point = 'pulses'))
  )
)
GROUP BY device_id, point
FORMAT JSONEachRow";

/// The first and last reading time of the tenant's power readings.
const REFERENCE_SPAN: &str = "SELECT toUnixTimestamp64Milli(min(ts)) AS first,
  toUnixTimestamp64Milli(max(ts)) AS last
FROM trustiot.telemetry WHERE tenant_id = {tenant:UUID}
FORMAT JSONEachRow";

/// A number of ClickHouse's JSON (a 64-bit integer may come as a string).
fn number(value: &Value) -> f64 {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
        .unwrap_or_else(|| panic!("a number: {value}"))
}

/// What a mark must show.
#[derive(Debug)]
enum Expected {
    /// A text, such as a device ID or a time.
    Text(String),
    /// A count: exactly this number.
    Count(f64),
    /// A measured value: this number, up to the last digits of a float sum computed in another
    /// order (the semantic API's mean is the cells' sums over the readings, Laysi/trustiot-v2#143).
    Value(f64),
}

impl Expected {
    fn text(value: &Value) -> Self {
        Self::Text(value.as_str().expect("a text").to_owned())
    }

    fn meets(&self, shown: &Value) -> bool {
        match self {
            Self::Text(text) => shown.as_str() == Some(text.as_str()),
            Self::Count(count) => shown.as_f64() == Some(*count),
            Self::Value(expected) => shown.as_f64().is_some_and(|value| {
                (value - expected).abs() <= 1e-9 * expected.abs().max(value.abs()).max(1.0)
            }),
        }
    }
}

/// The page: every cell of the four datasets and every computation as text marks, with labels
/// that hold no digits (a digit outside a mark is a problem of its own).
const POWER_PAGE: &str = r#"<style>
  table { border-collapse: collapse; margin: 8px 0; }
  td, th { padding: 4px 8px; text-align: right; }
</style>
<h1>Power and energy</h1>
<p>Peak <b id="peak_kw"></b> kW, mean <b id="mean_kw"></b> kW, spread <b id="spread_kw"></b> kW;
  first meter's hourly peak <b id="hourly_peak_kw"></b> kW; energy per reading
  <b id="kwh_per_interval"></b> kWh; pulses <b id="pulses_total"></b>.</p>
<table><tbody id="power_raw"></tbody></table>
<table><tbody id="power_hourly"></tbody></table>
<table><tbody id="power_stats"></tbody></table>
<table><tbody id="energy"></tbody></table>
<script>
  const COLUMNS = {
    power_raw: [["device_id"], ["readings", ",.0f"], ["low", ",.3f"], ["high", ",.3f"], ["total", ",.3f"]],
    power_hourly: [["hour", "%H:%M"], ["peak_kw", ",.3f"], ["mean_kw", ",.3f"]],
    power_stats: [["device_id"], ["count", ",.0f"], ["min", ",.3f"], ["max", ",.3f"], ["avg", ",.3f"], ["last", ",.3f"]],
    energy: [["device_id"], ["point"], ["total", ",.3f"], ["readings", ",.0f"]],
  };
  const COMPUTATIONS = [
    ["peak_kw", ",.3f"],
    ["mean_kw", ",.3f"],
    ["spread_kw", ",.3f"],
    ["hourly_peak_kw", ",.3f"],
    ["kwh_per_interval", ",.4f"],
    ["pulses_total", ",.0f"],
  ];
  tq.on("data", (tq) => {
    for (const [dataset, columns] of Object.entries(COLUMNS)) {
      const body = document.getElementById(dataset);
      body.replaceChildren();
      for (const row of tq.dataset(dataset).rows) {
        const tr = body.appendChild(document.createElement("tr"));
        for (const [column, format] of columns) {
          tq.text(tr.appendChild(document.createElement("td")), `c|${dataset}|${column}|${row.$row}`, format);
        }
      }
    }
    for (const [name, format] of COMPUTATIONS) {
      tq.text(document.getElementById(name), `f|${name}`, format);
    }
  });
</script>"#;

/// Criterion 1. The quantities the page shows, each compared with a direct query of
/// `trustiot.telemetry`:
///
/// - `power_raw` (SQL over the raw readings view, per meter): the device, the number of power
///   readings, the lowest, the highest and the sum;
/// - `power_hourly` (SQL over the hourly cells view, the first meter): each UTC hour, its highest
///   power and its mean (the cells' sum over their numeric readings), compared with the
///   readings grouped by `toStartOfHour`;
/// - `power_stats` (the statistics API over the whole range, both meters): the device, the count,
///   the minimum, the maximum, the mean and the latest reading;
/// - `energy` (the usage API over the whole range: the first meter's energy, cleared to zero once,
///   and the second meter's 32-bit pulse counter, which wraps): the device, the point, the total
///   and the readings, compared with the usage from consecutive readings;
/// - the computations: the highest power of all (`max`), the mean of all power readings (`sum`
///   over `sum`), the spread of the statistics (`max − min` across two columns), the first
///   meter's highest hourly peak, the first meter's energy per reading interval (cells and
///   arithmetic) and the pulse total (`last`).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the dashboard check image: DASHBOARD_CHECK_IMAGE=<image> cargo test -p assistant --test dashboard_acceptance -- --ignored"]
async fn the_numbers_on_a_dashboard_equal_clickhouses() {
    let _serial = SERIAL.lock().await;
    let Some(check) = CheckService::start().await else {
        return;
    };
    let Some(world) = world().await else {
        return;
    };
    let owner = world.person(TenantId::generate(), TenantRole::Editor);
    let tenant = owner.tenant();
    let Some(telemetry) = Telemetry::start(&world, tenant).await else {
        return;
    };

    // The synthetic dataset, ending within the last day (whole days, so every reading keeps its
    // place in its UTC hour).
    let dataset = Dataset::open(lab_dataset::synthetic_index()).expect("the synthetic lab dataset");
    let shift = dataset
        .shift_ending_before(Utc::now(), TimeDelta::days(1))
        .expect("a shift");
    let writer = lab_dataset::ClickHouse::new(
        &telemetry.clickhouse.url,
        &telemetry.clickhouse.admin.user,
        &telemetry.clickhouse.admin.password,
        None,
    )
    .expect("a ClickHouse writer");
    let loaded = dataset
        .load(
            &writer,
            &Target::new(*tenant.as_uuid(), Uuid::new_v4()),
            shift,
        )
        .await
        .expect("load the dataset");
    let meter_1 = loaded.device("meter-1").expect("meter-1").to_string();
    let meter_2 = loaded.device("meter-2").expect("meter-2").to_string();
    let tenant_param = tenant.to_string();

    // The whole range in whole UTC hours, so the semantic API reads the hourly cells.
    let span = telemetry
        .reference_rows(REFERENCE_SPAN, &[("param_tenant", &tenant_param)])
        .await;
    let at = |field: &str| -> DateTime<Utc> {
        let value = &span[0][field];
        let millis = value
            .as_i64()
            .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
            .unwrap_or_else(|| panic!("Unix milliseconds: {value}"));
        DateTime::from_timestamp_millis(millis)
            .expect("a time")
            .duration_trunc(TimeDelta::hours(1))
            .expect("an hour")
    };
    let (from, to) = (at("first"), at("last") + TimeDelta::hours(1));
    let range = |series: Value| {
        json!({
            "series": series,
            "tz": "UTC",
            "from": from.to_rfc3339(),
            "to": to.to_rfc3339(),
            "interval": "total",
        })
    };
    let write = json!({
        "action": "write",
        "title": "Power and energy",
        "datasets": {"upsert": [
            {
                "name": "power_raw",
                "title": "Power by meter",
                "description": "Each meter's power readings: count, lowest, highest and sum.",
                "max_rows": 10,
                "source": {"kind": "sql", "sql": "SELECT device_id, count() AS readings, \
                    min(value_float) AS low, max(value_float) AS high, sum(value_float) AS total \
                    FROM tenant_telemetry WHERE point = 'power_kw' GROUP BY device_id \
                    ORDER BY device_id"},
            },
            {
                "name": "power_hourly",
                "title": "First meter's power per hour",
                "description": "The highest and the mean power of each hour.",
                "max_rows": 24,
                "source": {"kind": "sql", "sql": format!("SELECT bucket AS hour, \
                    value_max AS peak_kw, value_sum / numeric_readings AS mean_kw \
                    FROM tenant_telemetry_1h WHERE device_id = '{meter_1}' \
                    AND point = 'power_kw' ORDER BY hour")},
            },
            {
                "name": "power_stats",
                "title": "Power statistics",
                "description": "The meters' power over the whole range.",
                "max_rows": 2,
                "source": {"kind": "telemetry_statistics", "request": range(json!([
                    {"device_id": meter_1, "point": "power_kw"},
                    {"device_id": meter_2, "point": "power_kw"},
                ]))},
            },
            {
                "name": "energy",
                "title": "Energy and pulses",
                "description": "The counters' usage over the whole range.",
                "max_rows": 2,
                "source": {"kind": "telemetry_usage", "request": range(json!([
                    {"device_id": meter_1, "point": "energy_kwh"},
                    {"device_id": meter_2, "point": "pulses"},
                ]))},
            },
        ]},
        "computations": {"upsert": [
            {"name": "peak_kw", "title": "Peak", "formula": "max(power_raw.high)", "unit": "kW"},
            {"name": "mean_kw", "title": "Mean", "formula": "sum(power_raw.total) / sum(power_raw.readings)", "unit": "kW"},
            {"name": "spread_kw", "title": "Spread", "formula": "max(power_stats.max) - min(power_stats.min)", "unit": "kW"},
            {"name": "hourly_peak_kw", "title": "Hourly peak", "formula": "max(power_hourly.peak_kw)", "unit": "kW"},
            {"name": "kwh_per_interval", "title": "Energy per reading", "formula": "energy.total[0] / (energy.readings[0] - 1)", "unit": "kWh"},
            {"name": "pulses_total", "title": "Pulses", "formula": "last(energy.total)"},
        ]},
        "page": POWER_PAGE,
        "locale": "en",
        "tz": "UTC",
    });
    let model = respond(move |_request, index| match index {
        0 => ok(step(0, &[("dashboard", write.clone())])),
        _ => ok(fixture("answer_end_turn.json")),
    });
    let tool = dashboard_tool(
        &world,
        &telemetry.url,
        &check.url,
        CHECK_CALL_TIMEOUT,
        DATASETS_TIMEOUT,
    );
    let (requests, _) = ask(
        &world,
        &owner,
        tool,
        model,
        "Put the meters' power and energy on a dashboard.",
    )
    .await;
    let saved = result_json(&requests[1]);
    assert_eq!(saved["saved"], true, "{saved}");
    let verdicts = check.verdicts();
    assert_eq!(verdicts.len(), 1, "{verdicts:?}");
    let (status, verdict) = &verdicts[0];
    assert_eq!(*status, 200, "{verdict}");
    assert_eq!(verdict["verdict"], "passed", "{verdict}");

    // The same quantities, straight from the readings.
    let tenant_only = [("param_tenant", tenant_param.as_str())];
    let power = telemetry
        .reference_rows(REFERENCE_POWER, &tenant_only)
        .await;
    let hourly = telemetry
        .reference_rows(
            REFERENCE_HOURLY,
            &[("param_tenant", &tenant_param), ("param_device", &meter_1)],
        )
        .await;
    let all = telemetry
        .reference_rows(REFERENCE_POWER_ALL, &tenant_only)
        .await;
    let usage = telemetry
        .reference_rows(
            REFERENCE_USAGE,
            &[
                ("param_tenant", &tenant_param),
                ("param_meter_1", &meter_1),
                ("param_meter_2", &meter_2),
            ],
        )
        .await;
    assert_eq!(power.len(), 2, "{power:?}");
    let of = |rows: &[Value], device: &str| -> Value {
        rows.iter()
            .find(|row| row["device_id"] == device)
            .cloned()
            .unwrap_or_else(|| panic!("{device} in {rows:?}"))
    };

    let mut expected: BTreeMap<String, Expected> = BTreeMap::new();
    for (index, row) in power.iter().enumerate() {
        let cell = |column: &str| format!("c|power_raw|{column}|{index}");
        expected.insert(cell("device_id"), Expected::text(&row["device_id"]));
        expected.insert(cell("readings"), Expected::Count(number(&row["readings"])));
        expected.insert(cell("low"), Expected::Value(number(&row["low"])));
        expected.insert(cell("high"), Expected::Value(number(&row["high"])));
        expected.insert(cell("total"), Expected::Value(number(&row["total"])));
    }
    for (index, row) in hourly.iter().enumerate() {
        let cell = |column: &str| format!("c|power_hourly|{column}|{index}");
        expected.insert(cell("hour"), Expected::text(&row["hour"]));
        expected.insert(cell("peak_kw"), Expected::Value(number(&row["peak_kw"])));
        expected.insert(cell("mean_kw"), Expected::Value(number(&row["mean_kw"])));
    }
    for (index, device) in [&meter_1, &meter_2].into_iter().enumerate() {
        let row = of(&power, device);
        let cell = |column: &str| format!("c|power_stats|{column}|{index}");
        expected.insert(cell("device_id"), Expected::Text(device.clone()));
        expected.insert(cell("count"), Expected::Count(number(&row["readings"])));
        expected.insert(cell("min"), Expected::Value(number(&row["low"])));
        expected.insert(cell("max"), Expected::Value(number(&row["high"])));
        expected.insert(cell("avg"), Expected::Value(number(&row["mean"])));
        expected.insert(cell("last"), Expected::Value(number(&row["last"])));
    }
    for (index, (device, point)) in [(&meter_1, "energy_kwh"), (&meter_2, "pulses")]
        .into_iter()
        .enumerate()
    {
        let row = of(&usage, device);
        assert_eq!(row["point"], point);
        let cell = |column: &str| format!("c|energy|{column}|{index}");
        expected.insert(cell("device_id"), Expected::Text(device.clone()));
        expected.insert(cell("point"), Expected::Text(point.to_owned()));
        expected.insert(cell("total"), Expected::Value(number(&row["total"])));
        expected.insert(cell("readings"), Expected::Count(number(&row["readings"])));
    }
    let all = &all[0];
    let first_meter = of(&power, &meter_1);
    let first_usage = of(&usage, &meter_1);
    let second_usage = of(&usage, &meter_2);
    for (name, value) in [
        ("peak_kw", number(&all["peak"])),
        ("mean_kw", number(&all["mean"])),
        ("spread_kw", number(&all["spread"])),
        ("hourly_peak_kw", number(&first_meter["high"])),
        ("kwh_per_interval", number(&first_usage["per_interval"])),
        ("pulses_total", number(&second_usage["total"])),
    ] {
        expected.insert(format!("f|{name}"), Expected::Value(value));
    }

    // Every mark of the report is one of these quantities, each is shown, and each equals
    // ClickHouse's.
    let marks = verdict["report"]["marks"]
        .as_array()
        .expect("the report's marks");
    let shown: BTreeMap<String, Value> = marks
        .iter()
        .map(|mark| {
            (
                mark["reference"].as_str().expect("a reference").to_owned(),
                mark["value"].clone(),
            )
        })
        .collect();
    assert_eq!(
        shown.len(),
        marks.len(),
        "one mark per reference: {marks:?}"
    );
    assert_eq!(
        shown.keys().collect::<Vec<_>>(),
        expected.keys().collect::<Vec<_>>(),
        "the report's references are the quantities"
    );
    let mut wrong = Vec::new();
    for (reference, want) in &expected {
        let value = &shown[reference];
        eprintln!("{reference}: dashboard {value}, ClickHouse {want:?}");
        if !want.meets(value) {
            wrong.push(format!(
                "{reference}: dashboard {value}, ClickHouse {want:?}"
            ));
        }
    }
    assert!(
        wrong.is_empty(),
        "values that differ:\n{}",
        wrong.join("\n")
    );
    assert_eq!(
        saved["check"]["marks"],
        marks.len(),
        "the tool reports the check's marks"
    );
    eprintln!(
        "{} marks equal ClickHouse's (synthetic dataset version {}, range {from} to {to})",
        marks.len(),
        dataset.index().version
    );
}
