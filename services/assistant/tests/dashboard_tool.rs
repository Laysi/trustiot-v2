//! The main agent's `dashboard` tool (Laysi/trustiot-v2#150) against the product's schema, with
//! stand-ins for the identity service, telemetry-query and the check service:
//!
//! - a write runs every dataset as the asking user (a token of their tenant, exchanged by the
//!   assistant), sends the public spec, the page and the results to the check, and writes a
//!   draft of this conversation only on a passing verdict; list and read show it, spec and page
//!   whole;
//! - a failing verdict, and every way of getting no usable verdict, saves nothing: the draft's
//!   latest version stays, and the model gets closed problem kinds with fixed hints, never the
//!   check service's text;
//! - the tool's refusals: another conversation's or user's draft, another tenant's dashboard, a
//!   stale or missing `base_version`, the 7th write of a question, oversized or malformed input;
//! - a saved dashboard becomes this conversation's draft, once; an archived one is refused; a
//!   rebase onto a newer saved version needs this question to have read that version;
//! - failed datasets are problems (SQL refusals with `telemetry_sql`'s hints), a revoked session
//!   ends the call, and a question with too little time left does not start a write;
//! - a question runs each dataset (source and `max_rows`) once.
//!
//! Starts a throwaway Postgres with TLS (podman or docker); skipped without either unless `CI`
//! is set.

mod check_stand_in;
mod dashboard_world;
mod support;

use std::net::TcpListener as StdListener;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use assistant::dashboards::checker::{CHECK_CALL_TIMEOUT, Checker};
use assistant::dashboards::store;
use assistant::tools::dashboard::{
    DATASETS_TIMEOUT, DashboardTool, MAX_WRITES_PER_QUESTION, SAVE_TIMEOUT, WriteBounds,
};
use assistant::tools::{Tool, ToolContent, ToolContext, ToolError, ToolOutput};
use check_stand_in::CheckStandIn;
use contract::api::dashboard::check::Problem;
use contract::api::dashboard::public::DashboardKind;
use contract::api::dashboard::spec::MAX_PAGE_BYTES;
use contract::{DashboardId, ServiceName, TenantId, TenantRole};
use dashboard_world::{
    Content, Peers, Person, Question, SQL_PATH, STATISTICS_PATH, World, dashboard_tool,
    dashboard_tool_with, spec, world,
};
use serde_json::{Value, json};
use support::Capture;
use tokio::time::Instant;
use uuid::Uuid;

/// In every error body the stand-ins send: it must reach neither the model nor the logs.
const SECRET: &str = "t150-service-secret";

/// The world, the stand-ins, the tool, and a user of a new tenant with a conversation.
struct Setup {
    world: World,
    peers: Peers,
    check: CheckStandIn,
    tool: Arc<DashboardTool>,
    owner: Person,
    conversation: Uuid,
}

async fn setup() -> Option<Setup> {
    let world = world().await?;
    let peers = Peers::start(Arc::clone(&world.issuer)).await;
    let check = CheckStandIn::start().await;
    let tool = tool(
        &world,
        &peers,
        &check.url,
        CHECK_CALL_TIMEOUT,
        DATASETS_TIMEOUT,
    );
    let owner = world.person(TenantId::generate(), TenantRole::Editor);
    let conversation = world.conversation(&owner).await;
    Some(Setup {
        world,
        peers,
        check,
        tool,
        owner,
        conversation,
    })
}

/// The tool with telemetry-query's stand-in ([`dashboard_tool`]).
fn tool(
    world: &World,
    peers: &Peers,
    check_url: &str,
    check_timeout: Duration,
    datasets_timeout: Duration,
) -> Arc<DashboardTool> {
    dashboard_tool(
        world,
        &peers.url,
        check_url,
        check_timeout,
        datasets_timeout,
    )
}

impl Setup {
    /// A new question of `person` in `conversation`.
    fn question(&self, person: &Person, conversation: Uuid) -> ToolContext {
        self.question_with(person, conversation, None, false)
    }

    fn question_with(
        &self,
        person: &Person,
        conversation: Uuid,
        deadline: Option<Instant>,
        revoked: bool,
    ) -> ToolContext {
        ToolContext {
            principal: person.principal(),
            conversation_id: conversation,
            run_id: Uuid::now_v7(),
            delegation: Arc::new(Question {
                issuer: Arc::clone(&self.world.issuer),
                claims: person.claims.clone(),
                deadline,
                revoked: AtomicBool::new(revoked),
            }),
        }
    }

    /// The owner's question in the owner's conversation.
    fn own_question(&self) -> ToolContext {
        self.question(&self.owner, self.conversation)
    }

    /// The latest version of `id` as `person` opens it: its number and page.
    async fn latest(&self, person: &Person, id: DashboardId) -> (u32, String) {
        let mut tx = self.world.tx(person).await;
        let opened = store::open(&mut tx, person.principal(), id, None)
            .await
            .expect("open the dashboard");
        tx.rollback().await.expect("roll back");
        (opened.version.version, opened.version.page)
    }
}

async fn call(
    tool: &Arc<DashboardTool>,
    context: &ToolContext,
    input: Value,
) -> Result<ToolOutput, ToolError> {
    Arc::clone(tool).call(context.clone(), input).await
}

fn text(output: &ToolOutput, index: usize) -> &str {
    match &output.content[index] {
        ToolContent::Text(text) => text,
        ToolContent::Image { .. } => panic!("an image"),
    }
}

fn json_of(output: &ToolOutput) -> Value {
    serde_json::from_str(text(output, 0)).expect("the result is JSON")
}

/// The input message of a refused call.
fn refused(result: Result<ToolOutput, ToolError>) -> String {
    match result {
        Err(ToolError::Input(message)) => message,
        other => panic!("expected an input error, got {other:?}"),
    }
}

/// A page longer than the tool result cap of other strings, so read shows it is not cut.
fn page(title: &str) -> String {
    format!(
        "<main><h1>{title}</h1><p id=\"rows\"></p><script>tq.on(\"data\", () => {{ \
         tq.text(document.getElementById(\"rows\"), \"m|daily|row_count\"); }});</script>{}\
         </main>",
        "<!-- the page's layout notes -->".repeat(12)
    )
}

fn datasets() -> Value {
    serde_json::to_value(spec().datasets).expect("datasets serialize")
}

fn new_dashboard(title: &str) -> Value {
    json!({
        "action": "write",
        "title": title,
        "description": "Energy and voltage of the plant.",
        "datasets": {"upsert": datasets()},
        "page": page(title),
    })
}

/// A write that changes `id`'s heading from `old` to `new`, on `base`.
fn edit(id: DashboardId, base: u32, old: &str, new: &str) -> Value {
    json!({
        "action": "write",
        "dashboard_id": id,
        "base_version": base,
        "page_edits": [{"old": format!("<h1>{old}</h1>"), "new": format!("<h1>{new}</h1>")}],
    })
}

/// Writes a new dashboard in the owner's conversation, which the check passes.
async fn first_version(setup: &Setup, title: &str) -> DashboardId {
    let output = call(&setup.tool, &setup.own_question(), new_dashboard(title))
        .await
        .expect("the write");
    let result = json_of(&output);
    assert!(!output.is_error, "{result}");
    serde_json::from_value(result["dashboard_id"].clone()).expect("a dashboard ID")
}

#[tokio::test]
async fn a_write_runs_every_dataset_as_the_user_and_saves_a_draft_only_after_the_check() {
    let Some(setup) = setup().await else {
        return;
    };
    let question = setup.own_question();
    let output = call(&setup.tool, &question, new_dashboard("Plant energy"))
        .await
        .expect("the write");
    let result = json_of(&output);
    eprintln!("a saved write: {result}");
    assert!(!output.is_error, "{result}");
    assert_eq!(result["saved"], true);
    assert_eq!(result["version"], 1);
    assert_eq!(result["kind"], "draft");
    assert_eq!(result["datasets"].as_array().map(Vec::len), Some(3));
    assert_eq!(result["datasets"][0]["row_count"], 3);
    assert_eq!(result["check"]["marks"], 3);
    let id: DashboardId = serde_json::from_value(result["dashboard_id"].clone()).expect("an ID");

    // Every dataset ran once at telemetry-query, as the user, exchanged by the assistant.
    let calls = setup.peers.calls();
    let mut paths: Vec<&str> = calls.iter().map(|call| call.path.as_str()).collect();
    paths.sort_unstable();
    assert_eq!(
        paths,
        [
            "/api/telemetry/v1/statistics",
            "/api/telemetry/v1/usage",
            SQL_PATH,
        ]
    );
    for call in &calls {
        assert_eq!(call.claims.sub, setup.owner.claims.sub);
        assert_eq!(call.tenant(), Some(setup.owner.tenant()));
        assert_eq!(call.claims.aud, ServiceName::TelemetryQuery);
        assert_eq!(
            call.claims.act.map(|actor| actor.sub),
            Some(ServiceName::Assistant)
        );
    }
    assert!(setup.peers.unexpected().is_empty());

    // The check got the page, the public spec (no SQL, no semantic request) and the results.
    let [request] = setup.check.requests().try_into().expect("one check");
    assert_eq!(request["page"], page("Plant energy"));
    assert_eq!(request["locale"], "zh-TW");
    assert_eq!(request["tz"], "UTC");
    assert_eq!(request["runtime"], 1);
    let names: Vec<&str> = request["datasets"]
        .as_array()
        .expect("results")
        .iter()
        .map(|result| result["dataset"].as_str().expect("a name"))
        .collect();
    assert_eq!(names, ["daily", "stats", "usage"]);
    assert!(!request["spec"].to_string().contains("SELECT"), "{request}");
    assert!(
        !request["spec"].to_string().contains("last_7_days"),
        "{request}"
    );

    // The draft is this conversation's, at version 1, with the page as written.
    let mut tx = setup.world.tx(&setup.owner).await;
    let opened = store::open(&mut tx, setup.owner.principal(), id, None)
        .await
        .expect("open the draft");
    tx.rollback().await.expect("roll back");
    assert_eq!(opened.dashboard.kind, DashboardKind::Draft);
    assert_eq!(opened.dashboard.conversation_id, Some(setup.conversation));
    assert_eq!(opened.version.version, 1);
    assert_eq!(opened.version.page, page("Plant energy"));
    assert_eq!(opened.version.title, "Plant energy");

    // list shows it; read gives the spec and the page whole.
    let listed = json_of(
        &call(&setup.tool, &question, json!({"action": "list"}))
            .await
            .expect("list"),
    );
    assert_eq!(listed["dashboards"][0]["dashboard_id"], json!(id));
    assert_eq!(listed["dashboards"][0]["kind"], "draft");
    assert_eq!(listed["more"], false);
    let read = call(
        &setup.tool,
        &question,
        json!({"action": "read", "dashboard_id": id}),
    )
    .await
    .expect("read");
    let about = json_of(&read);
    assert_eq!(about["version"], 1);
    assert_eq!(about["latest_version"], 1);
    assert_eq!(
        about["spec"]["datasets"][0]["source"]["sql"],
        json!(
            "SELECT toDate(time) AS day, sum(value) AS kwh FROM trustiot.tenant_telemetry GROUP BY day ORDER BY day"
        )
    );
    assert!(page("Plant energy").len() > 256);
    assert_eq!(text(&read, 1), page("Plant energy"));
}

#[tokio::test]
async fn a_failed_or_missing_verdict_saves_nothing_and_the_draft_keeps_its_version() {
    let Some(setup) = setup().await else {
        return;
    };
    let (capture, _guard) = Capture::install();
    let id = first_version(&setup, "Good").await;
    let mismatch: Problem = serde_json::from_value(json!({
        "kind": "mismatch", "reference": "c|daily|kwh|0", "value": 12.5, "tag": "span",
    }))
    .expect("a problem");
    let cases: [(&str, u16, String, &str); 10] = [
        (
            "a mismatching mark",
            200,
            json!({"verdict": "failed", "problems": [mismatch]}).to_string(),
            "mismatch",
        ),
        (
            "an unmapped shape",
            200,
            json!({"verdict": "failed", "problems": [{"kind": "unmapped", "tag": "rect"}]})
                .to_string(),
            "unmapped",
        ),
        (
            "a problem kind of a newer checker",
            200,
            json!({"verdict": "failed", "problems": [{"kind": "a_newer_kind", "note": SECRET}]})
                .to_string(),
            "unrecognized",
        ),
        (
            "a busy checker",
            503,
            json!({"note": SECRET}).to_string(),
            "check_busy",
        ),
        (
            "a refused request",
            400,
            json!({"note": SECRET}).to_string(),
            "check_refused",
        ),
        (
            "a failing checker",
            500,
            json!({"note": SECRET}).to_string(),
            "check_failed",
        ),
        (
            "an answer that is no verdict",
            200,
            format!("{SECRET} <html>"),
            "check_invalid_verdict",
        ),
        (
            "a report of a dataset the page does not have",
            200,
            json!({"verdict": "passed", "report": {"marks": [
                {"reference": "m|other|row_count", "value": 3},
            ]}})
            .to_string(),
            "check_invalid_verdict",
        ),
        (
            "a passed report with a value the data does not hold",
            200,
            json!({"verdict": "passed", "report": {"marks": [
                {"reference": "c|daily|kwh|0", "value": 13.5},
            ]}})
            .to_string(),
            "check_invalid_verdict",
        ),
        (
            "a mismatch with a value the data does not hold",
            200,
            json!({"verdict": "failed", "problems": [
                {"kind": "mismatch", "reference": "c|daily|kwh|0", "value": SECRET},
            ]})
            .to_string(),
            "check_invalid_verdict",
        ),
    ];
    for (case, status, body, kind) in cases {
        setup.check.answer(status, &body);
        let output = call(
            &setup.tool,
            &setup.own_question(),
            edit(id, 1, "Good", "Bad"),
        )
        .await
        .expect("the write");
        let result = json_of(&output);
        eprintln!("{case}: {result}");
        assert!(output.is_error, "{case}: {result}");
        assert_eq!(result["saved"], false, "{case}");
        let [problem] = result["problems"].as_array().expect("problems").as_slice() else {
            panic!("{case}: one problem: {result}");
        };
        assert_eq!(problem["kind"], kind, "{case}");
        assert!(
            problem["hint"]
                .as_str()
                .is_some_and(|hint| !hint.is_empty()),
            "{case}"
        );
        assert!(!text(&output, 0).contains(SECRET), "{case}");
        // The user still sees the good version.
        assert_eq!(
            setup.latest(&setup.owner, id).await,
            (1, page("Good")),
            "{case}"
        );
    }
    // The contract's fixed hint, with the problem's fields.
    setup.check.answer(
        200,
        &json!({"verdict": "failed", "problems": [mismatch]}).to_string(),
    );
    let output = call(
        &setup.tool,
        &setup.own_question(),
        edit(id, 1, "Good", "Bad"),
    )
    .await
    .expect("the write");
    let result = json_of(&output);
    assert_eq!(result["problems"][0]["hint"], mismatch.kind().hint());
    assert_eq!(result["problems"][0]["reference"], "c|daily|kwh|0");
    assert_eq!(result["problems"][0]["value"], 12.5);

    // A checker that cannot be reached.
    let closed = {
        let listener = StdListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("address").port();
        drop(listener);
        format!("http://127.0.0.1:{port}")
    };
    let unreachable = tool(
        &setup.world,
        &setup.peers,
        &closed,
        CHECK_CALL_TIMEOUT,
        DATASETS_TIMEOUT,
    );
    let output = call(
        &unreachable,
        &setup.own_question(),
        edit(id, 1, "Good", "Bad"),
    )
    .await
    .expect("the write");
    assert_eq!(json_of(&output)["problems"][0]["kind"], "check_unreachable");
    assert_eq!(setup.latest(&setup.owner, id).await, (1, page("Good")));

    // A checker that takes the connection and never answers: no verdict within the timeout this
    // test configures for one check.
    let silent = StdListener::bind("127.0.0.1:0").expect("bind");
    let silent_url = format!(
        "http://127.0.0.1:{}",
        silent.local_addr().expect("address").port()
    );
    let slow = tool(
        &setup.world,
        &setup.peers,
        &silent_url,
        Duration::from_secs(2),
        DATASETS_TIMEOUT,
    );
    let output = call(&slow, &setup.own_question(), edit(id, 1, "Good", "Bad"))
        .await
        .expect("the write");
    assert_eq!(json_of(&output)["problems"][0]["kind"], "check_timeout");
    assert_eq!(setup.latest(&setup.owner, id).await, (1, page("Good")));
    drop(silent);

    // The service's text reached no log line.
    for event in capture.events() {
        let line = format!("{} {:?}", event.message, event.fields);
        assert!(!line.contains(SECRET), "{line}");
    }
}

/// What the model learns from a verdict comes from closed sets, from checks against what the
/// tool sent (the page text, the declared names), or from the tool's own copy of the data:
/// never a script's message, an axis tick's value, a literal the page text does not hold, a row
/// past the result's end, or a limit the service names.
#[tokio::test]
async fn the_model_learns_only_closed_values_and_its_own_data_from_a_verdict() {
    let Some(setup) = setup().await else {
        return;
    };
    let id = first_version(&setup, "Good").await;
    // Values a check service chose, none of them in the page or the data the tool sent.
    let chosen = [
        "31415.9265",
        "27182.818",
        "16180.339",
        "8675309",
        "1999",
        "31415",
    ];
    let failed = json!({"verdict": "failed", "problems": [
        {"kind": "script_error", "message": format!("TypeError: {SECRET}")},
        {"kind": "script_error", "message": format!("{SECRET}: thrown")},
        {"kind": "script_error"},
        {"kind": "unmapped", "reference": "t|daily|kwh|31415.9265", "tag": "text"},
        {"kind": "unmapped", "reference": "c|daily|kwh|1999", "tag": "span"},
        {"kind": "mismatch", "reference": "t|daily|kwh|27182.818", "value": 27182.818},
        {"kind": "mark_geometry", "reference": "t|daily|kwh|16180.339", "channel": "axis"},
        {"kind": "literal_number", "place": "text", "literal": "8675309", "tag": "p"},
        {"kind": "literal_number", "place": "text", "literal": "1", "tag": "h1"},
        {"kind": "too_large", "limit": "elements", "max": 31415},
        {"kind": "mismatch", "reference": "c|daily|kwh|0", "value": 12.5, "tag": "span"},
    ]});
    setup.check.answer(200, &failed.to_string());
    let output = call(
        &setup.tool,
        &setup.own_question(),
        edit(id, 1, "Good", "Bad"),
    )
    .await
    .expect("the write");
    let result = json_of(&output);
    eprintln!("a failed verdict: {result}");
    assert_eq!(result["saved"], false);
    let without_hints: Vec<Value> = result["problems"]
        .as_array()
        .expect("problems")
        .iter()
        .map(|problem| {
            assert!(
                problem["hint"]
                    .as_str()
                    .is_some_and(|hint| !hint.is_empty()),
                "{problem}"
            );
            let mut problem = problem.clone();
            problem.as_object_mut().expect("an object").remove("hint");
            problem
        })
        .collect();
    let axis = json!({"dataset": "daily", "column": "kwh"});
    assert_eq!(
        without_hints,
        [
            json!({"kind": "script_error", "error_class": "TypeError"}),
            // The two errors without a known class are one problem.
            json!({"kind": "script_error", "error_class": "other"}),
            json!({"kind": "unmapped", "axis": axis, "tag": "text"}),
            json!({
                "kind": "unmapped",
                "row_past_end": {"dataset": "daily", "column": "kwh", "row_count": 3},
                "tag": "span",
            }),
            json!({"kind": "mismatch", "axis": axis}),
            json!({"kind": "mark_geometry", "axis": axis, "channel": "axis"}),
            json!({"kind": "literal_number", "place": "text", "tag": "p"}),
            json!({"kind": "literal_number", "place": "text", "literal": "1", "tag": "h1"}),
            json!({"kind": "too_large", "limit": "elements", "max": 20_000}),
            json!({
                "kind": "mismatch",
                "reference": "c|daily|kwh|0",
                "value": 12.5,
                "tag": "span",
            }),
        ]
    );
    let shown = text(&output, 0);
    assert!(!shown.contains(SECRET), "{shown}");
    for value in chosen {
        assert!(!shown.contains(value), "{value}: {shown}");
    }
    assert_eq!(setup.latest(&setup.owner, id).await, (1, page("Good")));

    // A passed write shows its marked values from the tool's own data, without the axis ticks.
    setup.check.answer(
        200,
        &json!({"verdict": "passed", "report": {"marks": [
            {"reference": "t|daily|kwh|16180.339", "value": 16180.339},
            {"reference": "c|daily|kwh|0", "value": 12.5},
            {"reference": "m|daily|row_count", "value": 3},
        ]}})
        .to_string(),
    );
    let output = call(
        &setup.tool,
        &setup.own_question(),
        edit(id, 1, "Good", "Better"),
    )
    .await
    .expect("the write");
    let result = json_of(&output);
    assert_eq!(result["saved"], true, "{result}");
    assert_eq!(
        result["check"],
        json!({"marks": 2, "values": [
            {"reference": "c|daily|kwh|0", "value": 12.5},
            {"reference": "m|daily|row_count", "value": 3},
        ]})
    );
    let shown = text(&output, 0);
    for value in chosen {
        assert!(!shown.contains(value), "{value}: {shown}");
    }
}

/// One tenant has one write in its datasets and check at a time (per replica): another write of
/// the tenant waits up to the bound this test sets and is then `check_busy` without running
/// anything or reaching the check, and is not counted against its question; other tenants'
/// writes go on meanwhile.
#[tokio::test]
async fn a_tenant_checks_one_write_at_a_time_and_other_tenants_go_on() {
    let Some(setup) = setup().await else {
        return;
    };
    let wait = Duration::from_secs(1);
    let tool = dashboard_tool_with(
        &setup.world,
        &setup.peers.url,
        Checker::new(&setup.check.url, CHECK_CALL_TIMEOUT).expect("a checker"),
        WriteBounds {
            write_wait: wait,
            ..WriteBounds::default()
        },
    );
    let held = setup.check.hold("Held");
    let first = tokio::spawn({
        let tool = Arc::clone(&tool);
        let question = setup.own_question();
        async move { call(&tool, &question, new_dashboard("Held")).await }
    });
    setup.check.wait_requests(1).await;
    let runs = setup.peers.calls().len();

    // Another member of the tenant, in another conversation.
    let colleague = setup.world.person(setup.owner.tenant(), TenantRole::Viewer);
    let conversation = setup.world.conversation(&colleague).await;
    let question = setup.question(&colleague, conversation);
    let started = Instant::now();
    let output = call(&tool, &question, new_dashboard("Second"))
        .await
        .expect("the write");
    let result = json_of(&output);
    assert_eq!(result["saved"], false, "{result}");
    assert_eq!(result["problems"][0]["kind"], "check_busy", "{result}");
    assert!(
        started.elapsed() >= wait,
        "it waited for the write in progress"
    );
    assert_eq!(
        setup.check.requests().len(),
        1,
        "it never reached the check"
    );
    assert_eq!(setup.peers.calls().len(), runs, "it ran no dataset");

    // Another tenant's write is checked and saved meanwhile.
    let other = setup.world.person(TenantId::generate(), TenantRole::Editor);
    let other_conversation = setup.world.conversation(&other).await;
    let output = call(
        &tool,
        &setup.question(&other, other_conversation),
        new_dashboard("Other"),
    )
    .await
    .expect("the write");
    assert_eq!(json_of(&output)["saved"], true, "{}", text(&output, 0));

    // Released, the first write saves, and the tenant's next write goes through.
    held.release();
    let first = first.await.expect("the first write").expect("the write");
    assert_eq!(json_of(&first)["saved"], true, "{}", text(&first, 0));
    let output = call(&tool, &question, new_dashboard("Second"))
        .await
        .expect("the write");
    assert_eq!(json_of(&output)["saved"], true, "{}", text(&output, 0));

    // The busy write was not counted: the question still has its other writes.
    let id: DashboardId =
        serde_json::from_value(json_of(&output)["dashboard_id"].clone()).expect("an ID");
    let mut heading = "Second".to_owned();
    for base in 1..MAX_WRITES_PER_QUESTION {
        let next = format!("Second {}", base + 1);
        let output = call(&tool, &question, edit(id, base, &heading, &next))
            .await
            .expect("the write");
        assert_eq!(json_of(&output)["saved"], true, "{}", text(&output, 0));
        heading = next;
    }
}

/// The checker client's places hold for all tenants: with one place, a write of another
/// tenant waits up to the bound this test sets and is then `check_busy` without reaching the
/// check.
#[tokio::test]
async fn the_checks_of_all_tenants_share_the_places() {
    let Some(setup) = setup().await else {
        return;
    };
    let wait = Duration::from_secs(1);
    let tool = dashboard_tool_with(
        &setup.world,
        &setup.peers.url,
        Checker::new(&setup.check.url, CHECK_CALL_TIMEOUT)
            .expect("a checker")
            .with_places(1, wait),
        WriteBounds::default(),
    );
    let held = setup.check.hold("Held");
    let first = tokio::spawn({
        let tool = Arc::clone(&tool);
        let question = setup.own_question();
        async move { call(&tool, &question, new_dashboard("Held")).await }
    });
    setup.check.wait_requests(1).await;
    let other = setup.world.person(TenantId::generate(), TenantRole::Editor);
    let conversation = setup.world.conversation(&other).await;
    let started = Instant::now();
    let output = call(
        &tool,
        &setup.question(&other, conversation),
        new_dashboard("Other"),
    )
    .await
    .expect("the write");
    let result = json_of(&output);
    assert_eq!(result["problems"][0]["kind"], "check_busy", "{result}");
    assert!(started.elapsed() >= wait, "it waited for a place");
    assert_eq!(
        setup.check.requests().len(),
        1,
        "it never reached the check"
    );
    held.release();
    let first = first.await.expect("the first write").expect("the write");
    assert_eq!(json_of(&first)["saved"], true, "{}", text(&first, 0));
}

/// The writes of all tenants share the replica's places for writes in progress: with one, a
/// write of another tenant waits up to the bound this test sets and is then `check_busy`
/// without running a dataset or reaching the check, and is not counted against its question.
#[tokio::test]
async fn the_writes_of_all_tenants_share_the_places_in_flight() {
    let Some(setup) = setup().await else {
        return;
    };
    let wait = Duration::from_secs(1);
    let tool = dashboard_tool_with(
        &setup.world,
        &setup.peers.url,
        Checker::new(&setup.check.url, CHECK_CALL_TIMEOUT).expect("a checker"),
        WriteBounds {
            write_wait: wait,
            writes_in_flight: 1,
            ..WriteBounds::default()
        },
    );
    let held = setup.check.hold("Held");
    let first = tokio::spawn({
        let tool = Arc::clone(&tool);
        let question = setup.own_question();
        async move { call(&tool, &question, new_dashboard("Held")).await }
    });
    setup.check.wait_requests(1).await;
    let runs = setup.peers.calls().len();
    let other = setup.world.person(TenantId::generate(), TenantRole::Editor);
    let conversation = setup.world.conversation(&other).await;
    let question = setup.question(&other, conversation);
    let started = Instant::now();
    let output = call(&tool, &question, new_dashboard("Other"))
        .await
        .expect("the write");
    let result = json_of(&output);
    assert_eq!(result["saved"], false, "{result}");
    assert_eq!(result["problems"][0]["kind"], "check_busy", "{result}");
    assert!(started.elapsed() >= wait, "it waited for a place");
    assert_eq!(
        setup.check.requests().len(),
        1,
        "it never reached the check"
    );
    assert_eq!(setup.peers.calls().len(), runs, "it ran no dataset");
    held.release();
    let first = first.await.expect("the first write").expect("the write");
    assert_eq!(json_of(&first)["saved"], true, "{}", text(&first, 0));

    // The busy write was not counted: the question still has all its writes.
    let output = call(&tool, &question, new_dashboard("Other"))
        .await
        .expect("the write");
    assert_eq!(json_of(&output)["saved"], true, "{}", text(&output, 0));
    let id: DashboardId =
        serde_json::from_value(json_of(&output)["dashboard_id"].clone()).expect("an ID");
    let mut heading = "Other".to_owned();
    for base in 1..MAX_WRITES_PER_QUESTION {
        let next = format!("Other {}", base + 1);
        let output = call(&tool, &question, edit(id, base, &heading, &next))
            .await
            .expect("the write");
        assert_eq!(json_of(&output)["saved"], true, "{}", text(&output, 0));
        heading = next;
    }
}

#[tokio::test]
async fn the_tool_refuses_what_this_question_may_not_write() {
    let Some(setup) = setup().await else {
        return;
    };
    let id = first_version(&setup, "Mine").await;

    // Another conversation of the same user, another user of the tenant, another tenant: the
    // draft does not exist for them.
    let other_conversation = setup.world.conversation(&setup.owner).await;
    let colleague = setup.world.person(setup.owner.tenant(), TenantRole::Admin);
    let colleague_conversation = setup.world.conversation(&colleague).await;
    let stranger = setup.world.person(TenantId::generate(), TenantRole::Admin);
    let stranger_conversation = setup.world.conversation(&stranger).await;
    for (who, context) in [
        (
            "another conversation",
            setup.question(&setup.owner, other_conversation),
        ),
        (
            "another user",
            setup.question(&colleague, colleague_conversation),
        ),
        (
            "another tenant",
            setup.question(&stranger, stranger_conversation),
        ),
    ] {
        let read = refused(
            call(
                &setup.tool,
                &context,
                json!({"action": "read", "dashboard_id": id}),
            )
            .await,
        );
        assert!(
            read.contains("no dashboard with this dashboard_id"),
            "{who}: {read}"
        );
        let write = refused(call(&setup.tool, &context, edit(id, 1, "Mine", "Taken")).await);
        assert!(
            write.contains("no dashboard with this dashboard_id"),
            "{who}: {write}"
        );
        let listed = json_of(
            &call(&setup.tool, &context, json!({"action": "list"}))
                .await
                .expect("list"),
        );
        assert_eq!(listed["dashboards"], json!([]), "{who}");
    }
    assert_eq!(setup.latest(&setup.owner, id).await, (1, page("Mine")));

    // A stale base_version, and none.
    let question = setup.own_question();
    let output = call(&setup.tool, &question, edit(id, 1, "Mine", "Second"))
        .await
        .expect("the write");
    assert_eq!(json_of(&output)["version"], 2);
    let stale = refused(call(&setup.tool, &question, edit(id, 1, "Second", "Third")).await);
    assert!(
        stale.contains("latest version is 2, not base_version 1"),
        "{stale}"
    );
    let mut without_base = edit(id, 2, "Second", "Third");
    without_base
        .as_object_mut()
        .expect("an object")
        .remove("base_version");
    let missing = refused(call(&setup.tool, &question, without_base).await);
    assert!(missing.contains("Give `base_version`"), "{missing}");

    // Malformed and oversized input runs nothing.
    let calls_before = setup.peers.calls().len();
    let checks_before = setup.check.requests().len();
    for input in [
        json!({"action": "write", "title": "T", "tenant_id": TenantId::generate()}),
        json!({"action": "publish", "dashboard_id": id}),
        json!({"action": "read", "dashboard_id": id, "user_id": "someone"}),
    ] {
        let message = refused(call(&setup.tool, &question, input.clone()).await);
        assert!(
            message.starts_with("The input does not follow the schema"),
            "{input}"
        );
    }
    let both = refused(
        call(
            &setup.tool,
            &question,
            json!({"action": "write", "dashboard_id": id, "base_version": 2,
                "page": page("x"), "page_edits": [{"old": "a", "new": "b"}]}),
        )
        .await,
    );
    assert!(both.contains("not both"), "{both}");
    let mut oversized = new_dashboard("Too big");
    oversized["page"] = json!(format!("<p>{}</p>", "x".repeat(MAX_PAGE_BYTES)));
    let mut nine = new_dashboard("Too many");
    let one = datasets()[0].clone();
    nine["datasets"]["upsert"] = json!(
        (0..9)
            .map(|index| {
                let mut dataset = one.clone();
                dataset["name"] = json!(format!("d{index}"));
                dataset
            })
            .collect::<Vec<_>>()
    );
    for (case, input, path) in [
        ("a page over the limit", oversized, "/page"),
        ("nine datasets", nine, "/spec/datasets"),
    ] {
        let output = call(&setup.tool, &question, input)
            .await
            .expect("the write");
        let result = json_of(&output);
        assert!(output.is_error, "{case}");
        assert_eq!(result["saved"], false, "{case}");
        let paths: Vec<&str> = result["problems"]
            .as_array()
            .expect("problems")
            .iter()
            .filter_map(|problem| problem["path"].as_str())
            .collect();
        assert!(
            paths.iter().any(|found| found.starts_with(path)),
            "{case}: {result}"
        );
    }
    assert_eq!(setup.peers.calls().len(), calls_before);
    assert_eq!(setup.check.requests().len(), checks_before);

    // A question writes at most six times; the seventh is refused before anything runs.
    setup.check.answer(
        200,
        &json!({"verdict": "failed", "problems": [{"kind": "not_rendered"}]}).to_string(),
    );
    let busy = setup.own_question();
    let calls_before = setup.peers.calls().len();
    let checks_before = setup.check.requests().len();
    for write in 1..=MAX_WRITES_PER_QUESTION {
        let output = call(
            &setup.tool,
            &busy,
            edit(id, 2, "Second", &format!("Try {write}")),
        )
        .await
        .expect("the write");
        assert_eq!(json_of(&output)["problems"][0]["kind"], "not_rendered");
    }
    let seventh = refused(call(&setup.tool, &busy, edit(id, 2, "Second", "Try 7")).await);
    assert!(seventh.contains("6 times"), "{seventh}");
    // The datasets ran for the first write only; each write was checked.
    assert_eq!(setup.peers.calls().len(), calls_before + 3);
    assert_eq!(
        setup.check.requests().len(),
        checks_before + usize::try_from(MAX_WRITES_PER_QUESTION).expect("small")
    );
    // The next question may write again.
    setup.check.script(None);
    let output = call(
        &setup.tool,
        &setup.own_question(),
        edit(id, 2, "Second", "Third"),
    )
    .await
    .expect("the write");
    assert_eq!(json_of(&output)["version"], 3);
}

#[tokio::test]
async fn a_saved_dashboard_becomes_this_conversations_draft_once() {
    let Some(setup) = setup().await else {
        return;
    };
    let editor = setup.world.person(setup.owner.tenant(), TenantRole::Editor);
    let (_, _, saved) = setup.world.saved(&editor, &Content::new("Plant")).await;
    let question = setup.own_question();

    // Listed with the organisation's saved dashboards.
    let listed = json_of(
        &call(&setup.tool, &question, json!({"action": "list"}))
            .await
            .expect("list"),
    );
    assert_eq!(listed["dashboards"][0]["dashboard_id"], json!(saved));
    assert_eq!(listed["dashboards"][0]["kind"], "saved");

    let output = call(
        &setup.tool,
        &question,
        json!({"action": "write", "dashboard_id": saved, "base_version": 1,
            "title": "Plant, by line"}),
    )
    .await
    .expect("the write");
    let result = json_of(&output);
    assert!(!output.is_error, "{result}");
    let draft: DashboardId = serde_json::from_value(result["dashboard_id"].clone()).expect("ID");
    assert_ne!(draft, saved);
    assert_eq!(result["version"], 1);
    let read = json_of(
        &call(
            &setup.tool,
            &question,
            json!({"action": "read", "dashboard_id": draft}),
        )
        .await
        .expect("read"),
    );
    assert_eq!(read["kind"], "draft");
    assert_eq!(read["title"], "Plant, by line");
    assert_eq!(
        read["taken_from"],
        json!({"dashboard_id": saved, "version": 1})
    );
    // The saved dashboard did not change; publishing the draft is the user's.
    assert_eq!(setup.latest(&setup.owner, saved).await.0, 1);

    // The conversation has its draft of it: write there instead.
    let again = refused(
        call(
            &setup.tool,
            &question,
            json!({"action": "write", "dashboard_id": saved, "base_version": 1,
                "title": "Plant, again"}),
        )
        .await,
    );
    assert!(again.contains(&draft.to_string()), "{again}");

    // An archived saved dashboard is not taken.
    let mut tx = setup.world.tx(&editor).await;
    store::set_archived(&mut tx, editor.requester(), saved, true)
        .await
        .expect("archive");
    tx.commit().await.expect("commit");
    let elsewhere = setup.world.conversation(&setup.owner).await;
    let archived = refused(
        call(
            &setup.tool,
            &setup.question(&setup.owner, elsewhere),
            json!({"action": "write", "dashboard_id": saved, "base_version": 1,
                "title": "Plant, archived"}),
        )
        .await,
    );
    assert!(archived.contains("archived"), "{archived}");
}

#[tokio::test]
async fn a_rebase_names_a_saved_version_this_question_read() {
    let Some(setup) = setup().await else {
        return;
    };
    let editor = setup.world.person(setup.owner.tenant(), TenantRole::Editor);
    let (editor_conversation, editor_draft, saved) =
        setup.world.saved(&editor, &Content::new("Plant")).await;

    // The owner's question takes a draft of the saved dashboard's version 1.
    let output = call(
        &setup.tool,
        &setup.own_question(),
        json!({"action": "write", "dashboard_id": saved, "base_version": 1,
            "title": "Plant, by line"}),
    )
    .await
    .expect("the write");
    let result = json_of(&output);
    assert!(!output.is_error, "{result}");
    let draft: DashboardId = serde_json::from_value(result["dashboard_id"].clone()).expect("ID");
    // The editor publishes versions 2 and 3 meanwhile.
    for (base, title) in [(1, "Plant 2"), (2, "Plant 3")] {
        setup
            .world
            .write_draft(
                &editor,
                editor_conversation,
                editor_draft,
                base,
                None,
                &Content::new(title),
            )
            .await
            .expect("the editor's next version");
        setup
            .world
            .publish(&editor, editor_draft, base + 1)
            .await
            .expect("the editor publishes it");
    }

    // A question that has not read version 3 cannot say it merged it: refused before anything
    // runs, and nothing is written.
    let question = setup.own_question();
    let rebase = json!({"action": "write", "dashboard_id": draft, "base_version": 1,
        "rebase_saved_version": 3, "title": "Plant, by line, merged"});
    let message = refused(call(&setup.tool, &question, rebase.clone()).await);
    assert!(
        message.contains(&saved.to_string()) && message.contains("read"),
        "{message}"
    );
    // Neither does reading version 2, nor reading version 3 in another question.
    let read = |version: Option<u32>| {
        let mut input = json!({"action": "read", "dashboard_id": saved});
        if let Some(version) = version {
            input["version"] = json!(version);
        }
        input
    };
    call(&setup.tool, &question, read(Some(2)))
        .await
        .expect("read version 2");
    call(&setup.tool, &setup.own_question(), read(None))
        .await
        .expect("another question reads version 3");
    refused(call(&setup.tool, &question, rebase.clone()).await);
    // Reading a draft names no saved version either.
    call(
        &setup.tool,
        &question,
        json!({"action": "read", "dashboard_id": draft}),
    )
    .await
    .expect("read the draft");
    refused(call(&setup.tool, &question, rebase.clone()).await);
    assert_eq!(setup.latest(&setup.owner, draft).await.0, 1);
    assert_eq!(
        setup.check.requests().len(),
        1,
        "only the first write was checked"
    );

    // Once this question read version 3, the merged write is saved and the draft is based on it.
    call(&setup.tool, &question, read(None))
        .await
        .expect("read the latest version");
    let output = call(&setup.tool, &question, rebase)
        .await
        .expect("the merged write");
    let result = json_of(&output);
    assert!(!output.is_error, "{result}");
    assert_eq!(result["version"], 2);
    let read = json_of(
        &call(
            &setup.tool,
            &question,
            json!({"action": "read", "dashboard_id": draft}),
        )
        .await
        .expect("read the draft"),
    );
    assert_eq!(
        read["taken_from"],
        json!({"dashboard_id": saved, "version": 3})
    );
}

#[tokio::test]
async fn failed_datasets_are_problems_and_a_revoked_session_ends_the_call() {
    let Some(setup) = setup().await else {
        return;
    };
    // telemetry-query refuses the SQL: the model gets telemetry_sql's hint for the class; a
    // refusal with anything more in it is not trusted and gets the general hint, without it.
    let details = Arc::new(Mutex::new(json!({"class": "not_a_select"})));
    let scripted = Arc::clone(&details);
    setup.peers.script(Arc::new(move |call| {
        let details = scripted.lock().expect("lock").clone();
        Box::pin(async move {
            (call.path == SQL_PATH).then(|| {
                (
                    422,
                    json!({
                        "code": "validation_failed",
                        "message_key": "error.validation_failed",
                        "details": details,
                    }),
                )
            })
        })
    }));
    for (case, refusal, hint) in [
        (
            "a refusal class",
            json!({"class": "not_a_select"}),
            "Only a SELECT",
        ),
        (
            "a refusal with more in it",
            json!({"class": "not_a_select", "message": SECRET}),
            "telemetry-query refused this dataset",
        ),
    ] {
        *details.lock().expect("lock") = refusal;
        let output = call(&setup.tool, &setup.own_question(), new_dashboard("Refused"))
            .await
            .expect("the write");
        let result = json_of(&output);
        eprintln!("{case}: {result}");
        assert!(output.is_error, "{case}");
        assert_eq!(result["saved"], false, "{case}");
        assert_eq!(result["problems"][0]["kind"], "dataset_refused", "{case}");
        assert_eq!(result["problems"][0]["dataset"], "daily", "{case}");
        assert!(
            result["problems"][0]["hint"]
                .as_str()
                .is_some_and(|found| found.starts_with(hint)),
            "{case}: {result}"
        );
        assert!(!text(&output, 0).contains(SECRET), "{case}");
    }
    // Nothing was checked.
    assert!(setup.check.requests().is_empty());

    // The user's session ended: the call ends, and the question with it.
    let revoked = setup.question_with(&setup.owner, setup.conversation, None, true);
    let ended = call(&setup.tool, &revoked, new_dashboard("Revoked")).await;
    assert_eq!(ended, Err(ToolError::Revoked));

    // Too little time left in the question to write.
    let late = setup.question_with(
        &setup.owner,
        setup.conversation,
        Some(Instant::now() + Duration::from_secs(5)),
        false,
    );
    let message = refused(call(&setup.tool, &late, new_dashboard("Late")).await);
    assert!(message.starts_with("Too little time"), "{message}");

    // None of them wrote anything.
    let mut tx = setup.world.tx(&setup.owner).await;
    let drafts = store::list_drafts(&mut tx, setup.owner.principal(), setup.conversation, None)
        .await
        .expect("list the drafts");
    tx.rollback().await.expect("roll back");
    assert!(drafts.dashboards.is_empty());
}

#[tokio::test]
async fn a_question_runs_each_dataset_once() {
    let Some(setup) = setup().await else {
        return;
    };
    let question = setup.own_question();
    let output = call(&setup.tool, &question, new_dashboard("Once"))
        .await
        .expect("the write");
    let id: DashboardId =
        serde_json::from_value(json_of(&output)["dashboard_id"].clone()).expect("ID");
    assert_eq!(setup.peers.calls().len(), 3);

    // Only the page changes: nothing runs again.
    let output = call(&setup.tool, &question, edit(id, 1, "Once", "Twice"))
        .await
        .expect("the write");
    assert_eq!(json_of(&output)["version"], 2);
    assert_eq!(setup.peers.calls().len(), 3);

    // One dataset changes: it alone runs.
    let mut daily = datasets()[0].clone();
    daily["max_rows"] = json!(1000);
    let output = call(
        &setup.tool,
        &question,
        json!({"action": "write", "dashboard_id": id, "base_version": 2,
            "datasets": {"upsert": [daily]}}),
    )
    .await
    .expect("the write");
    assert_eq!(json_of(&output)["version"], 3);
    assert_eq!(setup.peers.calls().len(), 4);

    // A dataset with another name but the same source and max_rows reuses the result.
    let mut copy = datasets()[1].clone();
    copy["name"] = json!("voltage_copy");
    let output = call(
        &setup.tool,
        &question,
        json!({"action": "write", "dashboard_id": id, "base_version": 3,
            "datasets": {"upsert": [copy]}}),
    )
    .await
    .expect("the write");
    assert_eq!(json_of(&output)["version"], 4);
    assert_eq!(setup.peers.calls().len(), 4);
    let requests = setup.check.requests();
    let last = requests.last().expect("a check");
    let results = last["datasets"].as_array().expect("results");
    assert_eq!(results.len(), 4);
    assert_eq!(results[3]["dataset"], "voltage_copy");
    assert_eq!(results[3]["rows"], results[1]["rows"]);

    // Another question runs them again.
    let output = call(
        &setup.tool,
        &setup.own_question(),
        edit(id, 4, "Twice", "Thrice"),
    )
    .await
    .expect("the write");
    assert_eq!(json_of(&output)["version"], 5);
    assert_eq!(setup.peers.calls().len(), 7);
}

#[tokio::test]
async fn datasets_still_running_at_the_bound_are_problems_and_the_rest_is_kept() {
    let Some(setup) = setup().await else {
        return;
    };
    // The call's limit covers each phase's own bound, so a phase ends with a result the model
    // can act on before the call is cut off.
    let limit = setup.tool.call_timeout().expect("the tool's own limit");
    assert!(
        limit > DATASETS_TIMEOUT + CHECK_CALL_TIMEOUT + SAVE_TIMEOUT,
        "{limit:?}"
    );

    // telemetry-query never answers the statistics dataset; the bound is what this test sets.
    let bounded = tool(
        &setup.world,
        &setup.peers,
        &setup.check.url,
        CHECK_CALL_TIMEOUT,
        Duration::from_secs(3),
    );
    setup.peers.script(Arc::new(|call| {
        Box::pin(async move {
            if call.path == STATISTICS_PATH {
                std::future::pending::<()>().await;
            }
            None
        })
    }));
    let question = setup.own_question();
    let output = call(&bounded, &question, new_dashboard("Bounded"))
        .await
        .expect("the write");
    let result = json_of(&output);
    assert!(output.is_error, "{result}");
    assert_eq!(result["saved"], false);
    let problems = result["problems"].as_array().expect("problems");
    // The statistics dataset is one of them; on a loaded machine the others may be too.
    let unfinished: Vec<&str> = problems
        .iter()
        .map(|problem| {
            assert_eq!(problem["kind"], "dataset_timeout", "{problem}");
            problem["dataset"].as_str().expect("the dataset")
        })
        .collect();
    assert!(unfinished.contains(&"stats"), "{result}");
    assert!(setup.check.requests().is_empty(), "nothing was checked");
    let calls_before = setup.peers.calls().len();

    // telemetry-query answers again: the question's next write runs only what did not finish.
    setup
        .peers
        .script(Arc::new(|_call| Box::pin(async { None })));
    let output = call(&bounded, &question, new_dashboard("Bounded"))
        .await
        .expect("the write");
    let result = json_of(&output);
    assert!(!output.is_error, "{result}");
    assert_eq!(
        setup.peers.calls().len() - calls_before,
        unfinished.len(),
        "only the datasets that did not finish ran again: {unfinished:?}"
    );
}
