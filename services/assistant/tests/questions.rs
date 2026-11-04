//! The question API (Laysi/trustiot-v2#646) end to end: the service binary
//! (`tests/service/mod.rs`) on the product's schema, verifying every request's identity token,
//! with a stand-in AI gateway whose model calls wait at a gate the test opens, and a stand-in
//! identity service that issues (or refuses) the questions' run delegations and counts them.
//!
//! A question asked through the API answers 202 at once and runs in the background; the test
//! reads the conversation until its `latest_run` has ended. A model call waits at the gate when
//! the question it answers contains [`HOLD`], so a test can stop or delete a question while it
//! runs; every other call is answered at once.
//!
//! Starts a throwaway Postgres with TLS (podman or docker); skipped without either unless `CI`
//! is set.

mod service;
mod support;

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use assistant::api::conversations::{
    CONVERSATION_PATH, CONVERSATIONS_PATH, DELETED_CONVERSATIONS_PATH, RESTORE_PATH,
};
use assistant::api::questions::{QUESTIONS_PATH, STOP_PATH};
use assistant::history::EntryKind;
use assistant::model::ModelClient;
use assistant::model::anthropic::AnthropicGateway;
use axum::Router;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use contract::api::identity::CreateRunDelegation;
use contract::client::Endpoint as _;
use contract::identity::TenantRole;
use contract::{ErrorBody, ErrorCode, TenantId};
use reqwest::Method;
use serde_json::{Value, json};
use service::{Reply, Service, User};
use service_kit::tenant::TenantDb;
use sqlx::PgConnection;
use sqlx::postgres::PgPoolOptions;
use support::{BoxFuture, Recorded, Responder, StandIn, WAIT, fixture, ok, settings};
use testkit::identity::TestIssuer;
use testkit::product::{DATABASE, ProductPostgres, product_postgres};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use uuid::Uuid;

/// A question that contains this waits at the gate for its model call.
const HOLD: &str = "[hold]";

/// Put in the identity service's refusals: it must reach neither the response nor the logs.
const REFUSAL_MARKER: &str = "identity-refusal-detail-5e1f";

/// Put in the AI gateway's error bodies: it must reach neither the conversation nor the logs.
const MODEL_MARKER: &str = "model-error-detail-9b2c";

/// The answer the fixture gives.
const ANSWER: &str = "Yes. All twelve meters on floor 3 reported in the last hour.";

/// The superuser moves a running question's start and deadline two hours back (the trigger
/// allows it: the run has not ended and its end time does not change).
const AGE_RUN: &str = "UPDATE assistant.runs
SET started_at = started_at - interval '2 hours', deadline_at = deadline_at - interval '2 hours'
WHERE run_id = $1::uuid";

/// A run as the superuser reads it: `$1` run.
const RUN_ROW: &str =
    "SELECT status, result_code, cancel_requested_at IS NOT NULL AS stop_requested,
    ended_at IS NOT NULL AS ended
FROM assistant.runs WHERE run_id = $1::uuid";

/// Every message of a conversation as the superuser reads it, bytes as hex: `$1` conversation.
const MESSAGES: &str = "SELECT coalesce(json_agg(m ORDER BY m.seq)::text, '[]')
FROM assistant.messages m WHERE m.conversation_id = $1::uuid";

fn unix_now() -> i64 {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("after the epoch");
    i64::try_from(elapsed.as_secs()).expect("seconds fit")
}

/// The stand-in identity service's run delegations: every request, and the refusal to answer.
#[derive(Default)]
struct Delegations {
    creates: Mutex<Vec<Value>>,
    refusal: Mutex<Option<ErrorCode>>,
}

/// The stand-in identity service.
struct Identity {
    url: String,
    state: Arc<Delegations>,
}

impl Identity {
    async fn start() -> Self {
        let state = Arc::new(Delegations::default());
        let router = Router::new()
            .route(CreateRunDelegation::ROUTE, post(create_delegation))
            .with_state(Arc::clone(&state));
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a loopback port");
        let address = listener.local_addr().expect("the local address");
        tokio::spawn(async move {
            axum::serve(listener, router)
                .await
                .expect("serve the stand-in identity service");
        });
        Self {
            url: format!("http://{address}"),
            state,
        }
    }

    /// How many delegations the service was asked for.
    fn creates(&self) -> usize {
        self.state
            .creates
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// The run IDs the delegations were asked for, in order.
    fn run_ids(&self) -> Vec<String> {
        self.state
            .creates
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|body| body["run_id"].as_str().unwrap_or_default().to_owned())
            .collect()
    }

    /// Refuses every delegation from now on with `code` (none: issues them).
    fn refuse(&self, code: Option<ErrorCode>) {
        *self
            .state
            .refusal
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = code;
    }
}

async fn create_delegation(
    State(state): State<Arc<Delegations>>,
    axum::Json(body): axum::Json<Value>,
) -> Response {
    let lifetime = body["lifetime_secs"].as_i64().unwrap_or_default();
    state
        .creates
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(body);
    let refusal = *state.refusal.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(code) = refusal {
        let body = ErrorBody::new(code).with_detail("reason", REFUSAL_MARKER);
        return (body.http_status(), axum::Json(body)).into_response();
    }
    axum::Json(json!({
        "delegation": "delegation.for.question",
        "expires_at": unix_now() + lifetime,
    }))
    .into_response()
}

/// The text of a model request's last message: the question it answers (no tools here).
fn last_message(request: &Recorded) -> String {
    request.json()["messages"]
        .as_array()
        .and_then(|messages| messages.last())
        .map(Value::to_string)
        .unwrap_or_default()
}

/// The stand-in AI gateway and the gate its held calls wait at.
struct Gateway {
    stand_in: StandIn,
    gate: Arc<Semaphore>,
}

impl Gateway {
    /// A gateway that answers each call with `answer`; a call for a question with [`HOLD`]
    /// waits for a permit of the gate first.
    async fn start(answer: impl Fn(&Recorded) -> support::Reply + Send + Sync + 'static) -> Self {
        let gate = Arc::new(Semaphore::new(0));
        let held = Arc::clone(&gate);
        let answer = Arc::new(answer);
        let responder: Responder = Arc::new(
            move |request: Recorded, _index: usize| -> BoxFuture<support::Reply> {
                let (gate, answer) = (Arc::clone(&held), Arc::clone(&answer));
                Box::pin(async move {
                    if last_message(&request).contains(HOLD) {
                        gate.acquire()
                            .await
                            .expect("the gate is never closed")
                            .forget();
                    }
                    answer(&request)
                })
            },
        );
        Self {
            stand_in: StandIn::start(responder).await,
            gate,
        }
    }

    /// Lets `calls` held calls through.
    fn open(&self, calls: usize) {
        self.gate.add_permits(calls);
    }

    fn requests(&self) -> Vec<Recorded> {
        self.stand_in.requests()
    }

    /// Waits until the gateway got `count` calls.
    async fn wait_calls(&self, count: usize) {
        let what = format!("{count} model calls at the stand-in gateway");
        service::wait_for(&what, WAIT, || async {
            (self.requests().len() >= count).then_some(())
        })
        .await;
    }
}

/// The recorded answer.
fn answer(_request: &Recorded) -> support::Reply {
    ok(fixture("answer_end_turn.json"))
}

struct World {
    product: Arc<ProductPostgres>,
    issuer: TestIssuer,
    gateway: Gateway,
    identity: Identity,
    env: Vec<(String, String)>,
    service: Service,
}

/// The service with the stand-ins, `env` on top; `None` without a container runtime.
async fn world_with(
    answer: impl Fn(&Recorded) -> support::Reply + Send + Sync + 'static,
    env: &[(&str, &str)],
) -> Option<World> {
    let product = product_postgres(&["assistant_app", "assistant_platform"]).await?;
    let issuer = TestIssuer::new();
    let gateway = Gateway::start(answer).await;
    let identity = Identity::start().await;
    let mut vars = vec![
        (
            "TRUSTIOT__AI_GATEWAY__URL".to_owned(),
            gateway.stand_in.url.clone(),
        ),
        (
            "TRUSTIOT__IDENTITY__EXCHANGE_URL".to_owned(),
            identity.url.clone(),
        ),
    ];
    vars.extend(
        env.iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned())),
    );
    let service = start(&product, &issuer, &vars).await;
    Some(World {
        product,
        issuer,
        gateway,
        identity,
        env: vars,
        service,
    })
}

async fn world() -> Option<World> {
    world_with(answer, &[]).await
}

async fn start(
    product: &ProductPostgres,
    issuer: &TestIssuer,
    env: &[(String, String)],
) -> Service {
    let env: Vec<(&str, &str)> = env
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect();
    Service::start(product, &issuer.jwks(), &env).await
}

fn questions(id: &str) -> String {
    QUESTIONS_PATH.replace("{conversation_id}", id)
}

fn stop_path(id: &str, run: &str) -> String {
    STOP_PATH
        .replace("{conversation_id}", id)
        .replace("{run_id}", run)
}

fn conversation_path(id: &str) -> String {
    CONVERSATION_PATH.replace("{conversation_id}", id)
}

impl World {
    /// Another replica on the same database and stand-ins.
    async fn replica(&self) -> Service {
        start(&self.product, &self.issuer, &self.env).await
    }

    /// A new conversation of `user`; its ID.
    async fn conversation(&self, user: &User) -> String {
        let reply = self
            .service
            .as_user(
                &self.issuer,
                user,
                Method::POST,
                CONVERSATIONS_PATH,
                Some(&json!({})),
            )
            .await;
        reply.expect_status(201);
        reply.json()["id"]
            .as_str()
            .expect("the conversation's ID")
            .to_owned()
    }

    async fn ask_on(&self, service: &Service, user: &User, id: &str, question: &str) -> Reply {
        service
            .as_user(
                &self.issuer,
                user,
                Method::POST,
                &questions(id),
                Some(&json!({ "question": question })),
            )
            .await
    }

    async fn ask(&self, user: &User, id: &str, question: &str) -> Reply {
        self.ask_on(&self.service, user, id, question).await
    }

    /// Asks `question` and returns its run's ID, after the 202.
    async fn asked(&self, user: &User, id: &str, question: &str) -> String {
        let reply = self.ask(user, id, question).await;
        reply.expect_status(202);
        reply.json()["id"]
            .as_str()
            .expect("the run's ID")
            .to_owned()
    }

    async fn stop_on(&self, service: &Service, user: &User, id: &str, run: &str) -> Reply {
        service
            .as_user(
                &self.issuer,
                user,
                Method::POST,
                &stop_path(id, run),
                Some(&json!({})),
            )
            .await
    }

    async fn read_on(&self, service: &Service, user: &User, id: &str) -> Value {
        let reply = service
            .as_user(
                &self.issuer,
                user,
                Method::GET,
                &conversation_path(id),
                None,
            )
            .await;
        reply.expect_status(200);
        reply.json()
    }

    async fn read(&self, user: &User, id: &str) -> Value {
        self.read_on(&self.service, user, id).await
    }

    /// Reads the conversation until its latest run is `run` with `status`; the conversation.
    async fn wait_run(&self, user: &User, id: &str, run: &str, status: &str) -> Value {
        let what = format!("question {run} reading as {status}");
        service::wait_for(&what, WAIT, || async {
            let detail = self.read(user, id).await;
            let latest = &detail["latest_run"];
            (latest["id"] == run && latest["status"] == status).then_some(detail)
        })
        .await
    }

    async fn superuser(&self) -> PgConnection {
        self.product.superuser().await
    }

    /// The run's status, failure code, whether a stop was requested, whether it has ended.
    async fn run_row(&self, run: &str) -> (String, Option<String>, bool, bool) {
        let mut admin = self.superuser().await;
        sqlx::query_as(RUN_ROW)
            .bind(run)
            .fetch_one(&mut admin)
            .await
            .expect("read the run")
    }

    /// Waits until the run's row has `status`.
    async fn wait_run_row(&self, run: &str, status: &str) -> (String, Option<String>, bool, bool) {
        let what = format!("the run {run} stored as {status}");
        service::wait_for(&what, WAIT, || async {
            let row = self.run_row(run).await;
            (row.0 == status).then_some(row)
        })
        .await
    }

    /// Every stored message of the conversation, bytes as hex.
    async fn messages(&self, id: &str) -> String {
        let mut admin = self.superuser().await;
        sqlx::query_scalar(MESSAGES)
            .bind(id)
            .fetch_one(&mut admin)
            .await
            .expect("read the messages")
    }

    /// Everything the service wrote to stdout and stderr.
    fn logs(&self, service: &Service) -> String {
        format!("{}\n{}", service.stdout.text(), service.stderr.text())
    }
}

/// The questions and answers of a conversation, as `(kind, text)`.
fn shown(detail: &Value) -> Vec<(String, String)> {
    detail["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .map(|message| {
            (
                message["kind"].as_str().unwrap_or_default().to_owned(),
                message["text"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect()
}

fn pair(kind: &str, text: &str) -> (String, String) {
    (kind.to_owned(), text.to_owned())
}

/// Acceptance criterion 1: a question answers 202 with its run at once and runs in the
/// background; reading the conversation shows it running, then the answer; the round is in the
/// history, and the next question's first model call starts with the previous call's messages,
/// byte for byte. The delegation is started with the run's own ID, and no log line carries the
/// question or the answer.
#[tokio::test(flavor = "multi_thread")]
async fn a_question_runs_in_the_background_and_its_answer_continues_the_history() {
    let Some(world) = world().await else {
        return;
    };
    let owner = User::new(TenantId::generate(), TenantRole::Viewer);
    let id = world.conversation(&owner).await;
    let question = format!("Did floor 3 report? {HOLD}");

    let reply = world.ask(&owner, &id, &question).await;
    reply.expect_status(202);
    assert_eq!(
        reply.header("location"),
        Some(conversation_path(&id).as_str())
    );
    assert_eq!(reply.header("cache-control"), Some("no-store"));
    let run = reply.json();
    let run_id = run["id"].as_str().expect("the run's ID").to_owned();
    assert_eq!(run["status"], "running", "{run}");
    assert_eq!(run["result"], Value::Null);
    assert_eq!(run["ended_at"], Value::Null);
    assert_eq!(run["stop_requested_at"], Value::Null);
    assert_eq!(world.identity.run_ids(), vec![run_id.clone()]);

    world.gateway.wait_calls(1).await;
    let running = world.read(&owner, &id).await;
    assert_eq!(running["latest_run"]["status"], "running");
    assert_eq!(running["messages"], json!([]), "nothing before the answer");

    world.gateway.open(1);
    let answered = world.wait_run(&owner, &id, &run_id, "answered").await;
    assert_eq!(
        shown(&answered),
        vec![pair("question", &question), pair("answer", ANSWER)]
    );
    assert_eq!(answered["latest_run"]["message_key"], Value::Null);
    assert!(answered["latest_run"]["ended_at"].is_string());
    assert_eq!(
        answered["title"],
        question.as_str(),
        "the provisional title: the question, under 40 characters"
    );
    assert_eq!(answered["title_source"], "provisional");

    let next = world.asked(&owner, &id, "And floor 4?").await;
    world.wait_run(&owner, &id, &next, "answered").await;
    let calls = world.gateway.requests();
    assert_eq!(calls.len(), 2);
    let (first, second) = (calls[0].raw_messages(), calls[1].raw_messages());
    assert_eq!(
        second.len(),
        first.len() + 2,
        "the answer and the next question"
    );
    assert_eq!(
        second[..first.len()],
        first[..],
        "the next call starts with the previous call's messages, byte for byte"
    );

    let started = world.service.wait_logged("question started").await;
    assert_eq!(started["fields"]["run_id"], run_id);
    let finished = world.service.wait_logged("question finished").await;
    assert_eq!(finished["fields"]["outcome"], "answered");
    let logs = world.logs(&world.service);
    for text in [question.as_str(), "And floor 4?", ANSWER, "twelve meters"] {
        assert!(!logs.contains(text), "{text:?} is in the logs");
    }
    println!("202 at once, then answered; the next call continued the history byte for byte");
}

/// Acceptance criterion 2: a stop ends the question: no further model call, the round is not in
/// the history, the run is `cancelled`. A stop again answers 409; the next question is answered
/// without the stopped round.
#[tokio::test(flavor = "multi_thread")]
async fn a_stop_ends_the_question_and_leaves_nothing_in_the_history() {
    let Some(world) = world().await else {
        return;
    };
    let owner = User::new(TenantId::generate(), TenantRole::Viewer);
    let id = world.conversation(&owner).await;
    let stopped_question = format!("Compare every meter for a year {HOLD}");
    let run = world.asked(&owner, &id, &stopped_question).await;
    world.gateway.wait_calls(1).await;

    let reply = world.stop_on(&world.service, &owner, &id, &run).await;
    reply.expect_status(202);
    let stopping = reply.json();
    assert_eq!(stopping["id"], run.as_str());
    assert!(stopping["stop_requested_at"].is_string(), "{stopping}");
    let cancelled = world.wait_run(&owner, &id, &run, "cancelled").await;
    assert_eq!(cancelled["latest_run"]["message_key"], "question.cancelled");
    assert_eq!(cancelled["messages"], json!([]));
    assert_eq!(
        world.gateway.requests().len(),
        1,
        "no model call after the stop"
    );
    let logged = world.service.wait_logged("question stop requested").await;
    assert_eq!(logged["fields"]["local"], true);
    assert_eq!(logged["fields"]["run_id"], run.as_str());

    world
        .stop_on(&world.service, &owner, &id, &run)
        .await
        .expect_error(409, "conflict", "question.run_ended");

    let next = world.asked(&owner, &id, "How much energy today?").await;
    let answered = world.wait_run(&owner, &id, &next, "answered").await;
    assert_eq!(
        shown(&answered),
        vec![
            pair("question", "How much energy today?"),
            pair("answer", ANSWER)
        ]
    );
    let calls = world.gateway.requests();
    assert_eq!(calls.len(), 2);
    assert!(
        !calls[1].body.contains("Compare every meter"),
        "the stopped question is not sent again"
    );
    assert_eq!(world.identity.creates(), 2);
    println!("stopped: cancelled at once, nothing stored, no further call; the next one answered");
}

/// Acceptance criterion 2, across replicas: a stop handled by another replica on the same
/// database reaches the replica that runs the question through its poll.
#[tokio::test(flavor = "multi_thread")]
async fn a_stop_through_another_replica_ends_the_question() {
    let Some(world) = world().await else {
        return;
    };
    let other = world.replica().await;
    let owner = User::new(TenantId::generate(), TenantRole::Viewer);
    let id = world.conversation(&owner).await;
    let run = world
        .asked(&owner, &id, &format!("Summarise last month {HOLD}"))
        .await;
    world.gateway.wait_calls(1).await;

    world
        .stop_on(&other, &owner, &id, &run)
        .await
        .expect_status(202);
    let logged = other.wait_logged("question stop requested").await;
    assert_eq!(
        logged["fields"]["local"], false,
        "the other replica runs it"
    );
    let cancelled = world.wait_run(&owner, &id, &run, "cancelled").await;
    assert_eq!(cancelled["messages"], json!([]));
    let finished = world.service.wait_logged("question finished").await;
    assert_eq!(finished["fields"]["outcome"], "cancelled");
    assert_eq!(world.gateway.requests().len(), 1);
    println!("a stop through the other replica cancelled the question within a poll");
}

/// Acceptance criterion 3: deleting a conversation while its question runs stops the question
/// (cancelled, its round not stored); the conversation is in the deleted list and comes back
/// byte for byte as it was before the question; another conversation's question asked meanwhile
/// is answered.
#[tokio::test(flavor = "multi_thread")]
async fn deleting_during_a_question_stops_it_and_a_restore_brings_back_the_conversation() {
    let Some(world) = world().await else {
        return;
    };
    let owner = User::new(TenantId::generate(), TenantRole::Viewer);
    let id = world.conversation(&owner).await;
    let first = world.asked(&owner, &id, "Did every meter report?").await;
    world.wait_run(&owner, &id, &first, "answered").await;
    let before = world.read(&owner, &id).await;
    let stored_before = world.messages(&id).await;
    let other = world.conversation(&owner).await;

    let run = world
        .asked(&owner, &id, &format!("Which meter used the most? {HOLD}"))
        .await;
    world.gateway.wait_calls(2).await;
    let meanwhile = world.asked(&owner, &other, "And the least?").await;

    world
        .service
        .as_user(
            &world.issuer,
            &owner,
            Method::DELETE,
            &conversation_path(&id),
            None,
        )
        .await
        .expect_status(204);
    let (status, _, stop_requested, ended) = world.wait_run_row(&run, "cancelled").await;
    assert_eq!(
        (status.as_str(), stop_requested, ended),
        ("cancelled", true, true)
    );
    let logged = world.service.wait_logged("question stop requested").await;
    assert_eq!(logged["fields"]["run_id"], run.as_str());
    world.wait_run(&owner, &other, &meanwhile, "answered").await;

    let deleted = world
        .service
        .as_user(
            &world.issuer,
            &owner,
            Method::GET,
            DELETED_CONVERSATIONS_PATH,
            None,
        )
        .await;
    deleted.expect_status(200);
    assert!(
        deleted.json()["conversations"]
            .as_array()
            .expect("conversations")
            .iter()
            .any(|conversation| conversation["id"] == id.as_str()),
        "in the deleted list"
    );
    world
        .service
        .as_user(
            &world.issuer,
            &owner,
            Method::POST,
            &RESTORE_PATH.replace("{conversation_id}", &id),
            Some(&json!({})),
        )
        .await
        .expect_status(200);
    assert_eq!(
        world.messages(&id).await,
        stored_before,
        "every stored message as before the question, byte for byte"
    );
    let restored = world.read(&owner, &id).await;
    assert_eq!(restored["messages"], before["messages"]);
    assert_eq!(restored["latest_run"]["status"], "cancelled");
    assert!(restored["latest_run"]["stop_requested_at"].is_string());
    println!(
        "deleted mid-question: cancelled, nothing stored, restored as before; the other answered"
    );
}

/// Acceptance criteria 4 and 5: no question starts in a deleted conversation, another user's,
/// another tenant's, an unknown one, or under an ID that is not a UUID (404), and none of them
/// calls the identity service; neither does a stop of a question that is not the caller's.
#[tokio::test(flavor = "multi_thread")]
async fn no_question_starts_where_the_caller_has_no_conversation() {
    let Some(world) = world().await else {
        return;
    };
    let tenant = TenantId::generate();
    let owner = User::new(tenant, TenantRole::Viewer);
    let colleague = User::new(tenant, TenantRole::Admin);
    let elsewhere = User {
        tenant: TenantId::generate(),
        ..owner
    };
    let id = world.conversation(&owner).await;
    let deleted = world.conversation(&owner).await;
    world
        .service
        .as_user(
            &world.issuer,
            &owner,
            Method::DELETE,
            &conversation_path(&deleted),
            None,
        )
        .await
        .expect_status(204);
    let unknown = Uuid::now_v7().to_string();
    for (who, conversation, what) in [
        (&owner, deleted.as_str(), "a deleted conversation"),
        (&colleague, id.as_str(), "another user's conversation"),
        (&elsewhere, id.as_str(), "the same user in another tenant"),
        (&owner, unknown.as_str(), "an unknown conversation"),
        (&owner, "not-a-uuid", "an ID that is not a UUID"),
    ] {
        let reply = world.ask(who, conversation, "Anything?").await;
        assert_eq!(reply.status, 404, "{what}: {}", reply.body);
        assert_eq!(reply.json()["code"], "not_found", "{what}");
    }
    let run = Uuid::now_v7().to_string();
    for (who, conversation) in [(&owner, id.as_str()), (&colleague, id.as_str())] {
        let reply = world.stop_on(&world.service, who, conversation, &run).await;
        assert_eq!(reply.status, 404, "{}", reply.body);
    }
    assert_eq!(world.identity.creates(), 0, "no delegation was asked for");
    assert!(world.gateway.requests().is_empty());
    println!("404 for every conversation that is not the caller's; no identity call");
}

/// Acceptance criterion 5: of two questions asked at once in one conversation, one starts (202)
/// and the other is refused (409) without calling the identity service.
#[tokio::test(flavor = "multi_thread")]
async fn two_questions_at_once_start_one() {
    let Some(world) = world().await else {
        return;
    };
    let owner = User::new(TenantId::generate(), TenantRole::Viewer);
    let id = world.conversation(&owner).await;
    let (first, second) = (format!("First {HOLD}"), format!("Second {HOLD}"));
    let (a, b) = tokio::join!(
        world.ask(&owner, &id, &first),
        world.ask(&owner, &id, &second),
    );
    let mut statuses = [a.status, b.status];
    statuses.sort_unstable();
    assert_eq!(statuses, [202, 409], "{} / {}", a.body, b.body);
    let refused = if a.status == 409 { &a } else { &b };
    refused.expect_error(409, "conflict", "conversation.question_running");
    assert_eq!(world.identity.creates(), 1, "only the started one");
    println!("two at once: one 202, one 409 conversation.question_running");
}

/// Acceptance criterion 6: when the identity service refuses the delegation, the request gets
/// the delegation's M1 error, the run is recorded `failed` with the code, the log has the code,
/// and the refusal's detail reaches neither the response nor the logs. The failed run does not
/// keep the next question from starting.
#[tokio::test(flavor = "multi_thread")]
async fn a_refused_delegation_answers_its_code_and_records_the_run_failed() {
    let Some(world) = world().await else {
        return;
    };
    let owner = User::new(TenantId::generate(), TenantRole::Viewer);
    let id = world.conversation(&owner).await;
    for (code, status, wire) in [
        (ErrorCode::Unauthenticated, 401, "unauthenticated"),
        (ErrorCode::Unavailable, 503, "unavailable"),
    ] {
        world.identity.refuse(Some(code));
        let reply = world.ask(&owner, &id, "How much energy?").await;
        assert_eq!(reply.status, status, "{}", reply.body);
        assert_eq!(reply.json()["code"], wire);
        assert!(!reply.body.contains(REFUSAL_MARKER), "{}", reply.body);
        let run = world.identity.run_ids().last().cloned().expect("asked");
        let detail = world.wait_run(&owner, &id, &run, "failed").await;
        assert_eq!(
            detail["latest_run"]["result"],
            format!("delegation_{wire}").as_str()
        );
        assert_eq!(detail["latest_run"]["message_key"], "question.not_started");
        assert!(!detail.to_string().contains(REFUSAL_MARKER));
    }
    // The lines reach the test through the child's stdout pipe, maybe after the responses.
    let logged = service::wait_for("two not-started lines in the log", WAIT, || async {
        let logged = world.service.stdout.messages(
            "question not started: the identity service did not issue its run delegation",
        );
        (logged.len() >= 2).then_some(logged)
    })
    .await;
    let codes: Vec<&str> = logged
        .iter()
        .filter_map(|line| line["fields"]["code"].as_str())
        .collect();
    assert_eq!(codes, ["unauthenticated", "unavailable"], "{logged:?}");
    assert!(world.gateway.requests().is_empty(), "no model call");

    world.identity.refuse(None);
    let run = world.asked(&owner, &id, "How much energy?").await;
    world.wait_run(&owner, &id, &run, "answered").await;
    let logs = world.logs(&world.service);
    assert!(
        !logs.contains(REFUSAL_MARKER),
        "the refusal's detail is in the logs"
    );
    println!("refused delegations: 401 and 503 with the code, runs failed, no detail anywhere");
}

/// Acceptance criterion 7: a question whose model call fails (several kinds) or is refused ends
/// `failed` with a fixed message key only; its round is not in the history, the gateway's text
/// reaches neither the conversation nor the logs, and the next question is answered.
#[tokio::test(flavor = "multi_thread")]
async fn model_failures_show_a_fixed_message_and_leave_nothing_in_the_history() {
    let error_body =
        json!({"type": "error", "error": {"type": "api_error", "message": MODEL_MARKER}})
            .to_string();
    let Some(world) = world_with(
        move |request| {
            let question = last_message(request);
            if question.contains("[fail 500]") {
                support::Reply::Json(500, error_body.clone())
            } else if question.contains("[fail 400]") {
                support::Reply::Json(400, error_body.clone())
            } else if question.contains("[fail 504]") {
                support::Reply::Json(504, error_body.clone())
            } else if question.contains("[refuse]") {
                ok(fixture("refusal.json"))
            } else {
                ok(fixture("answer_end_turn.json"))
            }
        },
        &[],
    )
    .await
    else {
        return;
    };
    let owner = User::new(TenantId::generate(), TenantRole::Viewer);
    let id = world.conversation(&owner).await;
    for (marker, kind) in [
        ("[fail 500]", "model_unavailable"),
        ("[fail 400]", "model_failed"),
        ("[fail 504]", "model_timeout"),
        ("[refuse]", "refused"),
    ] {
        let run = world
            .asked(&owner, &id, &format!("Question {marker}"))
            .await;
        let detail = world.wait_run(&owner, &id, &run, "failed").await;
        assert_eq!(detail["latest_run"]["result"], kind);
        assert_eq!(
            detail["latest_run"]["message_key"],
            format!("question.{kind}").as_str()
        );
        assert_eq!(detail["messages"], json!([]), "{marker}: nothing stored");
        assert!(!detail.to_string().contains(MODEL_MARKER));
    }
    let run = world.asked(&owner, &id, "Did every meter report?").await;
    let answered = world.wait_run(&owner, &id, &run, "answered").await;
    assert_eq!(
        shown(&answered),
        vec![
            pair("question", "Did every meter report?"),
            pair("answer", ANSWER)
        ]
    );
    let last = world.gateway.requests().pop().expect("the last call");
    assert_eq!(
        last.raw_messages().len(),
        1,
        "no failed round is sent again"
    );
    assert!(!world.logs(&world.service).contains(MODEL_MARKER));
    println!("four failures: fixed keys, nothing stored; the next question was answered");
}

/// Acceptance criterion 8: a question still `running` past its deadline (its replica died) reads
/// as `interrupted` and does not keep the next question from starting, which marks it so.
#[tokio::test(flavor = "multi_thread")]
async fn a_question_past_its_deadline_reads_as_interrupted_and_does_not_block_the_next() {
    let Some(world) = world().await else {
        return;
    };
    let owner = User::new(TenantId::generate(), TenantRole::Viewer);
    let id = world.conversation(&owner).await;
    let dying = world.replica().await;
    let reply = world
        .ask_on(&dying, &owner, &id, &format!("Long analysis {HOLD}"))
        .await;
    reply.expect_status(202);
    let run = reply.json()["id"].as_str().expect("the run").to_owned();
    world.gateway.wait_calls(1).await;
    drop(dying); // SIGKILL: no task records anything.
    let mut admin = world.superuser().await;
    sqlx::query(AGE_RUN)
        .bind(&run)
        .execute(&mut admin)
        .await
        .expect("move the run past its deadline");

    let detail = world.read(&owner, &id).await;
    assert_eq!(detail["latest_run"]["id"], run.as_str());
    assert_eq!(detail["latest_run"]["status"], "interrupted");
    assert_eq!(detail["latest_run"]["ended_at"], Value::Null);
    assert_eq!(detail["latest_run"]["message_key"], "question.interrupted");
    world
        .stop_on(&world.service, &owner, &id, &run)
        .await
        .expect_error(409, "conflict", "question.run_ended");

    let next = world.asked(&owner, &id, "Did every meter report?").await;
    world.wait_run(&owner, &id, &next, "answered").await;
    let (status, _, _, ended) = world.run_row(&run).await;
    assert_eq!((status.as_str(), ended), ("interrupted", true));
    println!("past its deadline: read as interrupted; the next question started and marked it");
}

/// Shutdown: SIGTERM ends a running question `interrupted` within the drain, and nothing of it
/// is stored.
#[tokio::test(flavor = "multi_thread")]
async fn shutting_down_marks_a_running_question_interrupted() {
    let Some(world) = world().await else {
        return;
    };
    let owner = User::new(TenantId::generate(), TenantRole::Viewer);
    let id = world.conversation(&owner).await;
    let mut stopping = world.replica().await;
    let reply = world
        .ask_on(&stopping, &owner, &id, &format!("Long analysis {HOLD}"))
        .await;
    reply.expect_status(202);
    let run = reply.json()["id"].as_str().expect("the run").to_owned();
    world.gateway.wait_calls(1).await;

    stopping.terminate();
    stopping.wait_exit(WAIT).await;
    let (status, code, _, ended) = world.run_row(&run).await;
    assert_eq!((status.as_str(), code, ended), ("interrupted", None, true));
    // The pipe may still hold the last lines when the process has exited.
    let finished = service::wait_for("the finished line in the log", WAIT, || async {
        let finished = stopping.stdout.messages("question finished");
        (!finished.is_empty()).then_some(finished)
    })
    .await;
    assert_eq!(
        finished.first().map(|line| &line["fields"]["outcome"]),
        Some(&json!("interrupted")),
        "{finished:?}"
    );
    let detail = world.read(&owner, &id).await;
    assert_eq!(detail["messages"], json!([]));
    assert_eq!(detail["latest_run"]["status"], "interrupted");
    assert!(detail["latest_run"]["ended_at"].is_string());
    println!("SIGTERM: the running question ended interrupted before the process exited");
}

/// A user has at most `questions.max_running_per_user` questions running in a tenant: of eight
/// asked at once in eight conversations, exactly three start, the rest are 429 without calling
/// the identity service. Another user is not affected.
#[tokio::test(flavor = "multi_thread")]
async fn a_user_runs_at_most_the_limit_of_questions_at_once() {
    let Some(world) = world_with(
        answer,
        &[("TRUSTIOT__QUESTIONS__MAX_RUNNING_PER_USER", "3")],
    )
    .await
    else {
        return;
    };
    let world = Arc::new(world);
    let tenant = TenantId::generate();
    let owner = User::new(tenant, TenantRole::Viewer);
    let mut conversations = Vec::new();
    for _ in 0..8 {
        conversations.push(world.conversation(&owner).await);
    }
    let mut asking = JoinSet::new();
    for id in conversations {
        let world = Arc::clone(&world);
        asking.spawn(async move { world.ask(&owner, &id, &format!("Analyse {HOLD}")).await });
    }
    let mut started = 0;
    let mut limited = 0;
    while let Some(reply) = asking.join_next().await {
        let reply = reply.expect("the request's task");
        match reply.status {
            202 => started += 1,
            429 => {
                reply.expect_error(429, "rate_limited", "question.running_limited");
                assert_eq!(reply.json()["details"]["max"], 3);
                limited += 1;
            }
            other => panic!("unexpected {other}: {}", reply.body),
        }
    }
    assert_eq!((started, limited), (3, 5));
    assert_eq!(world.identity.creates(), 3, "only the started ones");
    // The lines reach the test through the child's stdout pipe, maybe after the responses.
    let refusals = service::wait_for("five refusal lines in the log", WAIT, || async {
        let refusals = world
            .service
            .stdout
            .messages("question refused: the user reached the running limit");
        (refusals.len() >= 5).then_some(refusals)
    })
    .await;
    assert_eq!(refusals.len(), 5, "{refusals:?}");

    let colleague = User::new(tenant, TenantRole::Viewer);
    let id = world.conversation(&colleague).await;
    let run = world
        .asked(&colleague, &id, "Did every meter report?")
        .await;
    world.wait_run(&colleague, &id, &run, "answered").await;
    println!("eight at once: three started, five 429; another user unaffected");
}

/// A question is 1 to `questions.max_chars` characters after trimming; a longer one, a blank one
/// or a body larger than any question that fits is 422 on `/question`, a body that is not
/// `{"question": <string>}` is 400, and none of them calls the identity service.
#[tokio::test(flavor = "multi_thread")]
async fn a_question_is_one_to_max_characters_after_trimming() {
    let Some(world) = world_with(answer, &[("TRUSTIOT__QUESTIONS__MAX_CHARS", "10")]).await else {
        return;
    };
    let owner = User::new(TenantId::generate(), TenantRole::Viewer);
    let id = world.conversation(&owner).await;
    let path = questions(&id);
    let (service, issuer) = (&world.service, &world.issuer);
    let send = |body: Vec<u8>, content_type: &'static str| {
        let token = owner.token(issuer);
        let path = path.clone();
        async move {
            service
                .call(
                    Method::POST,
                    &path,
                    Some(&token),
                    Some((content_type, body)),
                )
                .await
        }
    };
    for question in ["01234567890", "   ", ""] {
        let reply = world.ask(&owner, &id, question).await;
        reply.expect_status(422);
        let body = reply.json();
        assert_eq!(body["code"], "validation_failed");
        assert_eq!(body["field_errors"][0]["field"], "/question");
        assert_eq!(
            body["field_errors"][0]["message_key"],
            "question.text_invalid"
        );
        assert_eq!(body["field_errors"][0]["details"]["max"], 10);
    }
    let oversized = format!("{{\"question\": \"{}\"}}", " ".repeat(10 * 12 + 1024));
    send(oversized.into_bytes(), "application/json")
        .await
        .expect_status(422);
    for body in [
        r#"{"text": "Hi"}"#,
        r#"{"question": 1}"#,
        r#"{"question": "Hi", "x": 1}"#,
        "[]",
    ] {
        send(body.as_bytes().to_vec(), "application/json")
            .await
            .expect_error(400, "invalid_request", "conversation.body_invalid");
    }
    let reply = send(br#"{"question": "Hi"}"#.to_vec(), "text/plain").await;
    assert_eq!(reply.status, 400, "{}", reply.body);
    assert_eq!(world.identity.creates(), 0);

    let run = world.asked(&owner, &id, "  0123456789\n").await;
    let detail = world.wait_run(&owner, &id, &run, "answered").await;
    assert_eq!(shown(&detail)[0], pair("question", "0123456789"), "trimmed");
    println!("ten characters after trimming start; eleven, blank and oversized are 422");
}

/// A conversation whose history holds `store::MAX_HISTORY_BYTES` or more takes no more
/// questions (409 `question.history_full`), without calling the identity service.
#[tokio::test(flavor = "multi_thread")]
async fn a_full_history_takes_no_more_questions() {
    let Some(world) = world().await else {
        return;
    };
    let owner = User::new(TenantId::generate(), TenantRole::Viewer);
    let agent = support::agent(&world.gateway.stand_in, Vec::new(), settings());
    let model =
        AnthropicGateway::new("http://127.0.0.1:9", "main", 1 << 20).expect("a valid gateway URL");
    let mut conversation = agent.start_conversation(owner.principal(), "");
    let long = "x".repeat(assistant::store::MAX_HISTORY_BYTES);
    conversation.append(
        EntryKind::Question,
        model.user_text(&long).expect("a message"),
    );
    let db = TenantDb::connect_lazy(
        world.product.postgres.options("assistant_app", DATABASE),
        PgPoolOptions::new().max_connections(1),
    )
    .expect("the service role's pool verifies the server");
    let mut tx = db
        .begin(&owner.identity(&world.issuer))
        .await
        .expect("a tenant transaction");
    assistant::store::create(&mut tx, &conversation)
        .await
        .expect("store the full conversation");
    tx.commit().await.expect("commit");

    world
        .ask(&owner, &conversation.id().to_string(), "One more?")
        .await
        .expect_error(409, "conflict", "question.history_full");
    assert_eq!(world.identity.creates(), 0);
    assert!(world.gateway.requests().is_empty());
    println!("a 16 MiB history: 409 question.history_full, no identity call");
}

/// What the answer guard removed from an answer is shown with it (`removed`), from the
/// database, so also after the service restarted; the answer's text has none of it.
#[tokio::test(flavor = "multi_thread")]
async fn an_answers_removed_content_is_shown_also_after_a_restart() {
    let text = "Floor 3 used 412 kWh yesterday.\n\n\
                ```sql\nSELECT sum(kwh) FROM tenant_telemetry_1h\n```\n\n\
                The total comes from <b>hourly</b> data.";
    // What the guard removes from that text, as the API and the column write it.
    let expected: Vec<String> = assistant::answer_guard::guard(text)
        .removed
        .kinds()
        .map(|kind| kind.as_str().to_owned())
        .collect();
    assert!(expected.len() >= 2, "code and HTML at least: {expected:?}");
    let mut recorded: Value =
        serde_json::from_str(&fixture("answer_end_turn.json")).expect("the fixture is JSON");
    let thinking = recorded["content"][0].clone();
    recorded["content"] = json!([thinking, {"type": "text", "text": text}]);
    let recorded = recorded.to_string();
    let Some(mut world) = world_with(move |_request| ok(recorded.clone()), &[]).await else {
        return;
    };
    let owner = User::new(TenantId::generate(), TenantRole::Viewer);
    let id = world.conversation(&owner).await;
    let run = world.asked(&owner, &id, "How much did floor 3 use?").await;
    let detail = world.wait_run(&owner, &id, &run, "answered").await;
    let messages = detail["messages"].as_array().expect("messages").clone();
    assert_eq!(messages.len(), 2);
    assert!(
        messages[0].get("removed").is_none(),
        "a question has no removed kinds"
    );
    assert_eq!(messages[1]["removed"], json!(expected));
    let text = messages[1]["text"].as_str().expect("the answer's text");
    for removed in ["SELECT", "```", "<b>", "tenant_telemetry"] {
        assert!(!text.contains(removed), "{removed:?} in {text:?}");
    }
    assert!(text.contains("412 kWh"));

    world.service.terminate();
    world.service.wait_exit(WAIT).await;
    let restarted = world.replica().await;
    let again = world.read_on(&restarted, &owner, &id).await;
    assert_eq!(
        again["messages"], detail["messages"],
        "the same after a restart"
    );
    let mut admin = world.superuser().await;
    let stored: Option<Vec<String>> = sqlx::query_scalar(
        "SELECT removed FROM assistant.messages WHERE conversation_id = $1::uuid AND kind = 'answer'",
    )
    .bind(&id)
    .fetch_one(&mut admin)
    .await
    .expect("read the column");
    assert_eq!(stored, Some(expected.clone()));
    world.service = restarted;
    println!("removed {expected:?} shown with the answer, also after a restart");
}

/// Decision 29 for the question API: the same user acting in another tenant cannot ask in, or
/// stop a question of, their conversation in the first tenant (404), and nothing changes; a stop
/// that names a run of another conversation is 404 too. The cross-tenant suite
/// (tests/cross_tenant.rs) exempts both endpoints for this test.
#[tokio::test(flavor = "multi_thread")]
async fn questions_and_stops_stay_in_their_tenant() {
    let Some(world) = world().await else {
        return;
    };
    let in_b = User::new(TenantId::generate(), TenantRole::Admin);
    let in_a = User {
        tenant: TenantId::generate(),
        ..in_b
    };
    let conversation_b = world.conversation(&in_b).await;
    let run_b = world
        .asked(
            &in_b,
            &conversation_b,
            &format!("Tenant B's question {HOLD}"),
        )
        .await;
    world.gateway.wait_calls(1).await;
    let conversation_a = world.conversation(&in_a).await;
    let second_b = world.conversation(&in_b).await;

    let reply = world.ask(&in_a, &conversation_b, "From tenant A").await;
    assert_eq!(reply.status, 404, "{}", reply.body);
    for (who, conversation) in [
        (&in_a, conversation_b.as_str()),
        (&in_a, conversation_a.as_str()),
        (&in_b, second_b.as_str()),
    ] {
        let reply = world
            .stop_on(&world.service, who, conversation, &run_b)
            .await;
        assert_eq!(reply.status, 404, "{conversation}: {}", reply.body);
    }
    assert_eq!(
        world.run_row(&run_b).await,
        ("running".to_owned(), None, false, false),
        "tenant B's question runs on, no stop requested"
    );
    assert_eq!(world.identity.creates(), 1, "only tenant B's own question");

    world
        .stop_on(&world.service, &in_b, &conversation_b, &run_b)
        .await
        .expect_status(202);
    world
        .wait_run(&in_b, &conversation_b, &run_b, "cancelled")
        .await;
    println!("tenant A reached nothing of tenant B's question; its owner there stopped it");
}

/// A user of a suspended tenant (Laysi/trustiot-v2#797) can neither ask, stop a running
/// question nor delete its conversation: each is refused before anything is read or written,
/// and the question runs on (it ends at its next token exchange, `tests/delegated_tools.rs`).
#[tokio::test(flavor = "multi_thread")]
async fn a_suspended_tenants_user_can_neither_ask_stop_nor_delete() {
    let Some(world) = world().await else {
        return;
    };
    let owner = User::new(TenantId::generate(), TenantRole::Admin);
    let id = world.conversation(&owner).await;
    let other = world.conversation(&owner).await;
    let run = world
        .asked(&owner, &id, &format!("Compare the floors {HOLD}"))
        .await;
    world.gateway.wait_calls(1).await;

    let mut claims = owner.claims(contract::ServiceName::Assistant);
    claims.tenant_suspended = true;
    let body = || Some(("application/json", br#"{"question": "Anything?"}"#.to_vec()));
    let empty = || Some(("application/json", b"{}".to_vec()));
    for (method, path, body, what) in [
        (Method::POST, stop_path(&id, &run), empty(), "stop"),
        (Method::DELETE, conversation_path(&id), None, "delete"),
        (Method::POST, questions(&other), body(), "ask"),
    ] {
        let token = world.issuer.token(&claims);
        let reply = world.service.call(method, &path, Some(&token), body).await;
        assert_eq!(reply.status, 403, "{what}: {}", reply.body);
        assert_eq!(reply.json()["code"], "tenant_suspended", "{what}");
    }
    assert_eq!(
        world.run_row(&run).await,
        ("running".to_owned(), None, false, false),
        "the question runs on, no stop requested"
    );
    assert_eq!(world.identity.creates(), 1, "no other question started");
    assert!(
        world.read(&owner, &id).await["latest_run"]["id"] == run.as_str(),
        "the conversation is not deleted"
    );

    world
        .stop_on(&world.service, &owner, &id, &run)
        .await
        .expect_status(202);
    world.wait_run(&owner, &id, &run, "cancelled").await;
}
