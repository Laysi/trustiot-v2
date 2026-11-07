//! Regenerating the last answer (Laysi/trustiot-v2#649) end to end: the service binary
//! (`tests/service/mod.rs`) on the product's schema, verifying every request's identity token,
//! with a stand-in AI gateway that answers every question with a tool step (thinking and a tool
//! call) and then an answer (thinking and text), and a stand-in identity service that issues
//! (or refuses) the questions' run delegations and counts them.
//!
//! A model call for a question that contains [`HOLD`] waits at a gate the test opens, so a test
//! can stop a regeneration, or keep a question running, while it runs. Every answer's text names
//! the call that gave it, so the test tells a regenerated answer from the discarded one.
//!
//! Starts a throwaway Postgres with TLS (podman or docker); skipped without either unless `CI`
//! is set.

mod service;
mod support;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use assistant::answer::ANSWER_TOOL;
use assistant::api::answer_stream::STREAM_PATH;
use assistant::api::conversations::{CONVERSATION_PATH, CONVERSATIONS_PATH};
use assistant::api::questions::{QUESTIONS_PATH, REGENERATE_PATH, STOP_PATH};
use assistant::api::sharing::{COPY_PATH, SHARE_PATH};
use assistant::history::{Conversation, EntryKind};
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
use uuid::Uuid;

/// A question that contains this waits at the gate for its first model call.
const HOLD: &str = "[hold]";

/// The messages one answered round stores here: the question, the tool step, its results, and
/// the answer.
const ROUND: usize = 4;

/// Every message of a conversation as the superuser reads it, discarded ones included, as JSON
/// rows (the body as `bytea` text, so byte for byte): `$1` conversation.
const MESSAGES: &str = "SELECT coalesce(json_agg(m ORDER BY m.seq)::text, '[]')
FROM assistant.messages m WHERE m.conversation_id = $1::uuid";

/// How many rounds of a conversation are discarded: `$1` conversation.
const DISCARDS: &str =
    "SELECT count(*) FROM assistant.discarded_rounds WHERE conversation_id = $1::uuid";

/// The position of the last answer of a conversation that no discarded round holds, as the
/// owner's `GET /conversations/{id}` names it (0 without one): `$1` conversation.
const LAST_ANSWER: &str = "SELECT coalesce(max(m.seq), 0) FROM assistant.messages m
WHERE m.conversation_id = $1::uuid AND m.kind = 'answer'
    AND NOT EXISTS (SELECT 1 FROM assistant.discarded_rounds r
                    WHERE r.conversation_id = m.conversation_id
                        AND m.seq BETWEEN r.from_seq AND r.to_seq)";

/// The superuser stores a round (question `$2`, answer `$3`, as JSON text) at positions 0 and
/// 1 of the empty conversation `$1`, bypassing the service's encoding.
const STORE_ROUND: &str = "INSERT INTO assistant.messages
    (tenant_id, conversation_id, seq, kind, format, body)
SELECT c.tenant_id, c.conversation_id, v.seq, v.kind, 'anthropic_messages',
    convert_to(v.body, 'UTF8')
FROM assistant.conversations c,
    (VALUES (0, 'question', $2), (1, 'answer', $3)) AS v (seq, kind, body)
WHERE c.conversation_id = $1::uuid";

/// The superuser stores `$3` conversations of user `$2` in tenant `$1`, started `$4` minutes
/// ago: a day's starts without making each one.
const SEED_STARTS: &str = "INSERT INTO assistant.conversations
    (tenant_id, conversation_id, user_id, prompt_version, system_prompt, tenant_context, tools,
     effort, cache_ttl, created_at, last_activity_at)
SELECT $1::uuid, gen_random_uuid(), $2::uuid, 'seed', '', '', '[]', 'medium', '5m',
    now() - make_interval(mins => $4), now() - make_interval(mins => $4)
FROM generate_series(1, $3)";

/// How many runs a conversation has: `$1` conversation.
const RUNS_OF: &str = "SELECT count(*) FROM assistant.runs WHERE conversation_id = $1::uuid";

/// The notes of a conversation that no discarded round holds: `$1` conversation.
const VISIBLE_NOTES: &str = "SELECT count(*) FROM assistant.messages m
WHERE m.conversation_id = $1::uuid AND m.kind = 'note'
    AND NOT EXISTS (SELECT 1 FROM assistant.discarded_rounds r
                    WHERE r.conversation_id = m.conversation_id
                        AND m.seq BETWEEN r.from_seq AND r.to_seq)";

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

    /// The run ID of every delegation the service was asked for, in order.
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
        let body = ErrorBody::new(code);
        return (body.http_status(), axum::Json(body)).into_response();
    }
    axum::Json(json!({
        "delegation": "delegation.for.question",
        "expires_at": unix_now() + lifetime,
    }))
    .into_response()
}

/// The last message of a model request.
fn last_message(request: &Recorded) -> Value {
    request.json()["messages"]
        .as_array()
        .and_then(|messages| messages.last())
        .cloned()
        .unwrap_or_default()
}

/// The answer text of the call `index`.
fn answer_text(index: usize) -> String {
    format!("Answer {index}: all twelve meters on floor 3 reported.")
}

/// The scripted model: a question (a user message whose content is its text) gets a tool step
/// with thinking; the step's results get an answer with thinking, whose text names the call.
fn scripted(request: &Recorded, index: usize) -> support::Reply {
    if last_message(request)["content"].is_string() {
        return ok(support::step(
            index,
            &[(
                "count_readings",
                json!({"meter": format!("floor-3-{index:04}"), "hours": 24}),
            )],
        ));
    }
    let mut answer: Value =
        serde_json::from_str(&fixture("answer_end_turn.json")).expect("the fixture is JSON");
    answer["content"][0]["signature"] = json!(format!("EuYBCkYICxIManswer{index:04}"));
    answer["content"][1]["text"] = json!(answer_text(index));
    ok(answer.to_string())
}

/// The stand-in AI gateway, the gate its held calls wait at, and the replies a test queued.
struct Gateway {
    stand_in: StandIn,
    gate: Arc<Semaphore>,
    queued: Arc<Mutex<VecDeque<support::Reply>>>,
}

impl Gateway {
    async fn start() -> Self {
        let gate = Arc::new(Semaphore::new(0));
        let held = Arc::clone(&gate);
        let queued = Arc::new(Mutex::new(VecDeque::new()));
        let replies = Arc::clone(&queued);
        let responder: Responder = Arc::new(
            move |request: Recorded, index: usize| -> BoxFuture<support::Reply> {
                let gate = Arc::clone(&held);
                let replies = Arc::clone(&replies);
                Box::pin(async move {
                    if last_message(&request).to_string().contains(HOLD) {
                        gate.acquire()
                            .await
                            .expect("the gate is never closed")
                            .forget();
                    }
                    let next = replies
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .pop_front();
                    next.unwrap_or_else(|| scripted(&request, index))
                })
            },
        );
        Self {
            stand_in: StandIn::start(responder).await,
            gate,
            queued,
        }
    }

    /// Answers the next call with `reply` instead of the scripted model.
    fn queue(&self, reply: support::Reply) {
        self.queued
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push_back(reply);
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

struct World {
    product: Arc<ProductPostgres>,
    issuer: TestIssuer,
    gateway: Gateway,
    identity: Identity,
    service: Service,
}

/// The service with the stand-ins, `env` on top; `None` without a container runtime.
async fn world_with(env: &[(&str, &str)]) -> Option<World> {
    let product = product_postgres(&["assistant_app", "assistant_platform"]).await?;
    let issuer = TestIssuer::new();
    let gateway = Gateway::start().await;
    let identity = Identity::start().await;
    let mut vars = vec![
        ("TRUSTIOT__AI_GATEWAY__URL", gateway.stand_in.url.as_str()),
        ("TRUSTIOT__IDENTITY__EXCHANGE_URL", identity.url.as_str()),
    ];
    vars.extend_from_slice(env);
    let service = Service::start(&product, &issuer.jwks(), &vars).await;
    Some(World {
        product,
        issuer,
        gateway,
        identity,
        service,
    })
}

async fn world() -> Option<World> {
    world_with(&[(ANSWER_TOOL_SWITCH, "false")]).await
}

fn path(template: &str, id: &str) -> String {
    template.replace("{conversation_id}", id)
}

/// The parts of a response a client can see, to compare two of them byte for byte.
fn seen(reply: &Reply) -> (u16, String, Option<String>, Option<String>) {
    (
        reply.status,
        reply.body.clone(),
        reply.header("content-type").map(str::to_owned),
        reply.header("cache-control").map(str::to_owned),
    )
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

/// The text of the answer a conversation shows last.
fn last_answer(detail: &Value) -> String {
    shown(detail).last().expect("a message").1.clone()
}

impl World {
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

    async fn ask(&self, user: &User, id: &str, question: &str) -> Reply {
        self.service
            .as_user(
                &self.issuer,
                user,
                Method::POST,
                &path(QUESTIONS_PATH, id),
                Some(&json!({ "question": question })),
            )
            .await
    }

    /// Asks `question` and returns its run's ID, after the 202.
    async fn asked(&self, user: &User, id: &str, question: &str) -> String {
        let reply = self.ask(user, id, question).await;
        reply.expect_status(202);
        run_id(&reply)
    }

    /// Asks `question` and waits until it is answered.
    async fn answered(&self, user: &User, id: &str, question: &str) -> Value {
        let run = self.asked(user, id, question).await;
        self.wait_run(user, id, &run, "answered").await
    }

    /// Regenerates naming the conversation's current last answer, as the database holds it
    /// (whoever `user` is: the request's refusals are what the tests look at).
    async fn regenerate(&self, user: &User, id: &str) -> Reply {
        let answer = self.last_answer_seq(id).await;
        self.regenerate_answer(user, id, answer).await
    }

    /// Regenerates naming the answer at `answer`.
    async fn regenerate_answer(&self, user: &User, id: &str, answer: i32) -> Reply {
        self.service
            .as_user(
                &self.issuer,
                user,
                Method::POST,
                &path(REGENERATE_PATH, id),
                Some(&json!({ "answer": answer })),
            )
            .await
    }

    /// The position of the conversation's last answer (0 without one, or for an ID that is not a
    /// UUID).
    async fn last_answer_seq(&self, id: &str) -> i32 {
        if Uuid::parse_str(id).is_err() {
            return 0;
        }
        let mut admin = self.superuser().await;
        sqlx::query_scalar(LAST_ANSWER)
            .bind(id)
            .fetch_one(&mut admin)
            .await
            .expect("read the last answer's position")
    }

    /// Regenerates and returns the new run's ID, after the 202.
    async fn regenerated(&self, user: &User, id: &str) -> String {
        let reply = self.regenerate(user, id).await;
        reply.expect_status(202);
        run_id(&reply)
    }

    async fn read(&self, user: &User, id: &str) -> Value {
        let reply = self
            .service
            .as_user(
                &self.issuer,
                user,
                Method::GET,
                &path(CONVERSATION_PATH, id),
                None,
            )
            .await;
        reply.expect_status(200);
        reply.json()
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

    /// Every stored message of the conversation, discarded ones included, byte for byte.
    async fn messages(&self, id: &str) -> Vec<Value> {
        let mut admin = self.superuser().await;
        let text: String = sqlx::query_scalar(MESSAGES)
            .bind(id)
            .fetch_one(&mut admin)
            .await
            .expect("read the messages");
        serde_json::from_str(&text).expect("the rows as JSON")
    }

    async fn count(&self, query: &'static str, id: &str) -> i64 {
        let mut admin = self.superuser().await;
        sqlx::query_scalar(query)
            .bind(id)
            .fetch_one(&mut admin)
            .await
            .expect("count")
    }

    async fn discards(&self, id: &str) -> i64 {
        self.count(DISCARDS, id).await
    }

    /// Stores `conversation` as the service would, through the store; its ID.
    async fn store(&self, user: &User, conversation: &Conversation) -> String {
        let db = TenantDb::connect_lazy(
            self.product.postgres.options("assistant_app", DATABASE),
            PgPoolOptions::new().max_connections(1),
        )
        .expect("the service role's pool verifies the server");
        let mut tx = db
            .begin(&user.identity(&self.issuer))
            .await
            .expect("a tenant transaction");
        assistant::store::create(&mut tx, conversation)
            .await
            .expect("store the conversation");
        tx.commit().await.expect("commit");
        conversation.id().to_string()
    }

    /// A conversation of `user` whose history is `entries`, each a kind and a text written as
    /// the service writes a question, an answer or a note; stored through the store, its ID.
    async fn conversation_of(&self, user: &User, entries: &[(EntryKind, &str)]) -> String {
        let agent = support::agent(&self.gateway.stand_in, Vec::new(), settings());
        let model = AnthropicGateway::new("http://127.0.0.1:9", "main", 32 << 20)
            .expect("a valid gateway URL");
        let mut conversation = agent.start_conversation(user.principal(), "");
        for (kind, text) in entries {
            let message = match kind {
                EntryKind::Question => model.user_text(text),
                EntryKind::Answer => model.assistant_text(text),
                _ => model.system_note(text),
            };
            conversation.append(*kind, message.expect("a message"));
        }
        self.store(user, &conversation).await
    }

    /// Everything the service wrote to stdout and stderr.
    fn logs(&self) -> String {
        format!(
            "{}\n{}",
            self.service.stdout.text(),
            self.service.stderr.text()
        )
    }
}

fn run_id(reply: &Reply) -> String {
    reply.json()["id"]
        .as_str()
        .expect("the run's ID")
        .to_owned()
}

/// Acceptance criterion 1 and the title: in a conversation of three rounds, each with a tool
/// step and thinking, regenerating the last answer leaves the first two rounds in the database
/// byte for byte; the new run's first model request is the original third round's first
/// request, byte for byte (the first two rounds' history, then the same question); once it is
/// answered the conversation shows one last answer, the new one, and its title is unchanged.
/// The log tells the regeneration from a question and carries no question or answer text.
#[tokio::test(flavor = "multi_thread")]
async fn regenerating_replaces_the_last_answer_and_nothing_before_it() {
    let Some(world) = world().await else {
        return;
    };
    let owner = User::new(TenantId::generate(), TenantRole::Viewer);
    let id = world.conversation(&owner).await;
    let questions = [
        "Did floor 3 report?",
        "Which meter used the most?",
        "And the least?",
    ];
    for question in questions {
        world.answered(&owner, &id, question).await;
    }
    let before = world.read(&owner, &id).await;
    let stored_before = world.messages(&id).await;
    assert_eq!(stored_before.len(), 3 * ROUND);
    let calls_before = world.gateway.requests();
    assert_eq!(
        calls_before.len(),
        6,
        "a step and an answer for each question"
    );
    let third_round_first_call = calls_before[4].raw_messages();
    let old_answer = last_answer(&before);
    assert_eq!(old_answer, answer_text(5));
    let positions: Vec<i64> = before["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .map(|message| message["seq"].as_i64().expect("a position"))
        .collect();
    assert_eq!(
        positions,
        [0, 3, 4, 7, 8, 11],
        "each question and answer at its stored position, the steps between them hidden"
    );
    let old_seq = world.last_answer_seq(&id).await;
    assert_eq!(old_seq, 11);

    let reply = world.regenerate(&owner, &id).await;
    reply.expect_status(202);
    assert_eq!(
        reply.header("location"),
        Some(path(CONVERSATION_PATH, &id).as_str())
    );
    assert_eq!(reply.header("cache-control"), Some("no-store"));
    let run = run_id(&reply);
    assert_eq!(reply.json()["status"], "running");
    let detail = world.wait_run(&owner, &id, &run, "answered").await;

    let calls = world.gateway.requests();
    assert_eq!(calls.len(), 8, "the regeneration's step and answer");
    assert_eq!(
        calls[6].raw_messages(),
        third_round_first_call,
        "the regeneration's first request is the third question's first, byte for byte"
    );
    let regenerated = calls[6].raw_messages();
    assert_eq!(
        regenerated.len(),
        2 * ROUND + 1,
        "two rounds of history, then the question"
    );
    assert_eq!(
        serde_json::from_str::<Value>(regenerated.last().expect("a message")).expect("JSON")["content"],
        questions[2]
    );

    let stored = world.messages(&id).await;
    assert_eq!(
        stored[..2 * ROUND],
        stored_before[..2 * ROUND],
        "the first two rounds, byte for byte"
    );
    assert_eq!(
        stored[..3 * ROUND],
        stored_before[..],
        "the discarded round stays stored until the conversation is purged"
    );
    assert_eq!(stored.len(), 4 * ROUND, "the new round after it");
    assert_eq!(world.discards(&id).await, 1);

    let mut expected = shown(&before);
    expected.pop();
    expected.push(pair("answer", &answer_text(7)));
    assert_eq!(shown(&detail), expected, "one last answer, the new one");
    assert_ne!(last_answer(&detail), old_answer);
    assert_eq!(detail["title"], before["title"], "the title stays");
    assert_eq!(detail["title"], questions[0]);
    assert_eq!(detail["title_source"], before["title_source"]);
    assert_eq!(
        detail["messages"][5]["seq"],
        4 * ROUND - 1,
        "the new answer at the end of the stored history"
    );

    // A second request naming the answer it already replaced (a double submit, a stale tab)
    // discards nothing.
    let stored_now = world.messages(&id).await;
    world
        .regenerate_answer(&owner, &id, old_seq)
        .await
        .expect_error(409, "conflict", "question.regenerate_stale");
    assert_eq!(world.messages(&id).await, stored_now, "nothing discarded");
    assert_eq!(world.discards(&id).await, 1);
    assert_eq!(world.gateway.requests().len(), 8, "no model call");

    let started = service::wait_for("the regeneration's start in the log", WAIT, || async {
        world
            .service
            .stdout
            .messages("question started")
            .into_iter()
            .find(|line| line["fields"]["run_id"] == run.as_str())
    })
    .await;
    assert_eq!(started["fields"]["regenerated"], true, "{started}");
    assert_eq!(started["fields"]["discarded_messages"], ROUND, "{started}");
    let asked = world.service.stdout.messages("question started");
    assert!(
        asked
            .iter()
            .filter(|line| line["fields"]["run_id"] != run.as_str())
            .all(|line| line["fields"]["regenerated"] == false),
        "{asked:?}"
    );
    let logs = world.logs();
    for text in questions
        .iter()
        .copied()
        .chain([old_answer.as_str(), "twelve meters"])
    {
        assert!(!logs.contains(text), "{text:?} is in the logs");
    }
    println!("regenerated: two rounds byte for byte, the same first request, one new answer");
}

/// Acceptance criterion 2: a regeneration is refused, with nothing discarded and no identity
/// call, while a question runs (409), when the last round is not an answered one (an empty
/// conversation, a last question without an answer: 409 `question.nothing_to_regenerate`), for
/// another user's, another tenant's, a deleted, an unknown conversation and an ID that is not a
/// UUID (404), for the readers of a shared conversation (viewer, editor and admin: 404 byte for
/// byte as for one that does not exist), and for a body that is not empty (400).
#[tokio::test(flavor = "multi_thread")]
async fn a_regeneration_is_refused_where_there_is_nothing_to_discard_or_no_right() {
    let Some(world) = world().await else {
        return;
    };
    let tenant = TenantId::generate();
    let owner = User::new(tenant, TenantRole::Viewer);
    let id = world.conversation(&owner).await;
    world.answered(&owner, &id, "Did floor 3 report?").await;
    let empty = world.conversation(&owner).await;
    let unanswered = world
        .conversation_of(
            &owner,
            &[
                (EntryKind::Question, "Did floor 3 report?"),
                (EntryKind::Answer, "Yes."),
                (EntryKind::Question, "And floor 4?"),
            ],
        )
        .await;
    let deleted = world.conversation(&owner).await;
    world
        .answered(&owner, &deleted, "Did floor 4 report?")
        .await;
    world
        .service
        .as_user(
            &world.issuer,
            &owner,
            Method::DELETE,
            &path(CONVERSATION_PATH, &deleted),
            None,
        )
        .await
        .expect_status(204);
    world
        .service
        .as_user(
            &world.issuer,
            &owner,
            Method::PUT,
            &path(SHARE_PATH, &id),
            None,
        )
        .await
        .expect_status(200);
    let stored = world.messages(&id).await;
    let creates = world.identity.creates();
    let calls = world.gateway.requests().len();

    for conversation in [&empty, &unanswered] {
        world.regenerate(&owner, conversation).await.expect_error(
            409,
            "conflict",
            "question.nothing_to_regenerate",
        );
        assert_eq!(world.discards(conversation).await, 0);
        assert_eq!(world.count(RUNS_OF, conversation).await, 0);
    }

    let unknown = Uuid::now_v7().to_string();
    let missing = world.regenerate(&owner, &unknown).await;
    assert_eq!(missing.status, 404, "{}", missing.body);
    assert_eq!(missing.json()["code"], "not_found");
    let elsewhere = User {
        tenant: TenantId::generate(),
        ..owner
    };
    for (who, conversation, what) in [
        (&owner, deleted.as_str(), "a deleted conversation"),
        (&elsewhere, id.as_str(), "the same user in another tenant"),
        (&owner, "not-a-uuid", "an ID that is not a UUID"),
    ] {
        let reply = world.regenerate(who, conversation).await;
        assert_eq!(seen(&reply), seen(&missing), "{what}");
    }
    for role in [TenantRole::Viewer, TenantRole::Editor, TenantRole::Admin] {
        let reader = User::new(tenant, role);
        let reply = world.regenerate(&reader, &id).await;
        assert_eq!(
            seen(&reply),
            seen(&missing),
            "a reader of the shared conversation ({role:?})"
        );
        let missing = world.regenerate(&reader, &unknown).await;
        assert_eq!(seen(&reply), seen(&missing), "{role:?}");
    }

    let token = owner.token(&world.issuer);
    let regenerate = path(REGENERATE_PATH, &id);
    let no_json = world
        .service
        .call(Method::POST, &regenerate, Some(&token), None)
        .await;
    assert_eq!(no_json.status, 400, "{}", no_json.body);
    assert_eq!(no_json.json()["code"], "invalid_request");
    // The answer must be named: no body, `null`, `{}` and anything else are refused.
    let answer = world.last_answer_seq(&id).await;
    for body in [
        "",
        "null",
        "{}",
        r#"{"question": "Something else?"}"#,
        r#"{"answer": -1}"#,
        r#"{"answer": "3"}"#,
        r#"{"answer": 3.5}"#,
        r#"{"answer": 2147483648}"#,
        r#"{"answer": 4294967295}"#,
        r#"{"answer": 4294967296}"#,
        format!(r#"{{"answer": {answer}, "question": "x"}}"#).as_str(),
        format!("[{answer}]").as_str(),
    ] {
        world
            .service
            .call(
                Method::POST,
                &regenerate,
                Some(&owner.token(&world.issuer)),
                Some(("application/json", body.as_bytes().to_vec())),
            )
            .await
            .expect_error(400, "invalid_request", "conversation.body_invalid");
    }
    // An answer that is not the last one: the question before it (a position that is no
    // answer) and a position past the end.
    for stale in [0, answer + 1] {
        world
            .regenerate_answer(&owner, &id, stale)
            .await
            .expect_error(409, "conflict", "question.regenerate_stale");
    }

    let held = world
        .asked(&owner, &id, &format!("Compare the floors {HOLD}"))
        .await;
    world.gateway.wait_calls(calls + 1).await;
    world.regenerate(&owner, &id).await.expect_error(
        409,
        "conflict",
        "conversation.question_running",
    );
    assert_eq!(
        world.identity.creates(),
        creates + 1,
        "only the held question asked for a delegation"
    );
    assert_eq!(world.messages(&id).await, stored, "nothing was discarded");
    assert_eq!(world.discards(&id).await, 0);
    assert_eq!(world.discards(&deleted).await, 0);

    world.gateway.open(1);
    world.wait_run(&owner, &id, &held, "answered").await;
    assert_eq!(world.discards(&id).await, 0);
    println!(
        "refused: 409 running, stale or nothing to regenerate, 404 for the rest, 400 for a body"
    );
}

/// Acceptance criterion 3: a regeneration stopped through the stop endpoint leaves neither the
/// discarded round nor the stopped one in the history, and the next question follows the first
/// two rounds: its first model request starts with them, byte for byte.
#[tokio::test(flavor = "multi_thread")]
async fn a_stopped_regeneration_leaves_neither_round() {
    let Some(world) = world().await else {
        return;
    };
    let owner = User::new(TenantId::generate(), TenantRole::Viewer);
    let id = world.conversation(&owner).await;
    world.answered(&owner, &id, "Did floor 3 report?").await;
    world
        .answered(&owner, &id, "Which meter used the most?")
        .await;
    let third = format!("Compare the floors {HOLD}");
    let run = world.asked(&owner, &id, &third).await;
    world.gateway.wait_calls(5).await;
    world.gateway.open(1);
    world.wait_run(&owner, &id, &run, "answered").await;
    let two_rounds = world.gateway.requests()[4].raw_messages()[..2 * ROUND].to_vec();
    let before = world.read(&owner, &id).await;

    let reply = world.regenerate(&owner, &id).await;
    reply.expect_status(202);
    assert_eq!(
        reply.json()["question"],
        third.as_str(),
        "the 202 names the question asked again (Laysi/trustiot-v2#1157)"
    );
    let regeneration = run_id(&reply);
    world.gateway.wait_calls(7).await;
    let running = world.read(&owner, &id).await;
    assert_eq!(running["latest_run"]["status"], "running");
    assert_eq!(
        running["latest_run"]["question"],
        third.as_str(),
        "a reader that did not send it reads the discarded round's question while it runs"
    );
    assert_eq!(
        shown(&running),
        shown(&before)[..2 * 2],
        "while it runs, the conversation ends with the second round"
    );
    world
        .service
        .as_user(
            &world.issuer,
            &owner,
            Method::POST,
            &STOP_PATH
                .replace("{conversation_id}", &id)
                .replace("{run_id}", &regeneration),
            Some(&json!({})),
        )
        .await
        .expect_status(202);
    let stopped = world
        .wait_run(&owner, &id, &regeneration, "cancelled")
        .await;
    assert_eq!(shown(&stopped), shown(&before)[..2 * 2]);
    assert_eq!(
        stopped["latest_run"]["question"],
        Value::Null,
        "the stop cleared the text"
    );
    assert_eq!(world.gateway.requests().len(), 7, "no call after the stop");

    let next = world.asked(&owner, &id, "And floor 4?").await;
    let answered = world.wait_run(&owner, &id, &next, "answered").await;
    let first = world.gateway.requests()[7].raw_messages();
    assert_eq!(first.len(), 2 * ROUND + 1);
    assert_eq!(
        first[..2 * ROUND],
        two_rounds[..],
        "the next question follows the first two rounds, byte for byte"
    );
    let shown_now = shown(&answered);
    assert_eq!(shown_now[..4], shown(&before)[..4]);
    assert_eq!(
        shown_now[4..],
        [
            pair("question", "And floor 4?"),
            pair("answer", &answer_text(8))
        ]
    );
    assert!(
        !shown_now.iter().any(|(_, text)| text == &third),
        "the discarded round's question is gone"
    );
    println!("a stopped regeneration: neither round in the history; the next one follows round 2");
}

/// A copy of a shared conversation (Laysi/trustiot-v2#650) whose last round is copied cannot be
/// regenerated (the user's decision of 2026-11-04): 409 `question.copied_answer`, its history
/// byte for byte as before, no identity call. Once the copier asked a question of their own,
/// that round regenerates, and the copy's note stays in the history the model gets.
#[tokio::test(flavor = "multi_thread")]
async fn a_copied_answer_is_not_regenerated_but_the_copiers_own_are() {
    let Some(world) = world().await else {
        return;
    };
    let tenant = TenantId::generate();
    let owner = User::new(tenant, TenantRole::Viewer);
    let copier = User::new(tenant, TenantRole::Viewer);
    let source = world.conversation(&owner).await;
    world.answered(&owner, &source, "Did floor 3 report?").await;
    world
        .answered(&owner, &source, "Which meter used the most?")
        .await;
    world
        .service
        .as_user(
            &world.issuer,
            &owner,
            Method::PUT,
            &path(SHARE_PATH, &source),
            None,
        )
        .await
        .expect_status(200);
    let copied = world
        .service
        .as_user(
            &world.issuer,
            &copier,
            Method::POST,
            &path(COPY_PATH, &source),
            Some(&json!({})),
        )
        .await;
    copied.expect_status(201);
    let copy = copied.json()["id"].as_str().expect("an ID").to_owned();
    let stored = world.messages(&copy).await;
    let kinds: Vec<&str> = stored
        .iter()
        .filter_map(|row| row["kind"].as_str())
        .collect();
    assert_eq!(
        kinds,
        ["question", "answer", "question", "note", "answer"],
        "the copy as Agent::copied_conversation writes it"
    );
    let creates = world.identity.creates();

    world
        .regenerate(&copier, &copy)
        .await
        .expect_error(409, "conflict", "question.copied_answer");
    assert_eq!(world.messages(&copy).await, stored, "byte for byte");
    assert_eq!(world.discards(&copy).await, 0);
    assert_eq!(world.count(RUNS_OF, &copy).await, 0);
    assert_eq!(world.identity.creates(), creates, "no identity call");

    let own = world.answered(&copier, &copy, "And floor 4?").await;
    let calls = world.gateway.requests().len();
    let run = world.regenerated(&copier, &copy).await;
    let regenerated = world.wait_run(&copier, &copy, &run, "answered").await;
    let mut expected = shown(&own);
    expected.pop();
    expected.push(pair("answer", &answer_text(calls + 1)));
    assert_eq!(shown(&regenerated), expected);
    assert_eq!(world.count(VISIBLE_NOTES, &copy).await, 1, "the note stays");
    assert_eq!(world.discards(&copy).await, 1);
    let requests = world.gateway.requests();
    let first = &requests[calls];
    let roles: Vec<String> = first.json()["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .map(|message| message["role"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert_eq!(
        roles,
        ["user", "assistant", "user", "system", "assistant", "user"],
        "the copied rounds with their note, then the copier's question again"
    );
    println!("a copied last round: 409 copied_answer; the copier's own round regenerated");
}

/// A full replica (503 `question.busy`) leaves the round in place: nothing discarded, no run
/// recorded, no identity call. Once a seat is free, the same request regenerates, and the
/// first round's provisional title stays.
#[tokio::test(flavor = "multi_thread")]
async fn a_full_replica_discards_nothing() {
    let Some(world) = world_with(&[("TRUSTIOT__QUESTIONS__MAX_RUNNING_PER_REPLICA", "1")]).await
    else {
        return;
    };
    let owner = User::new(TenantId::generate(), TenantRole::Viewer);
    let other = User::new(TenantId::generate(), TenantRole::Viewer);
    let id = world.conversation(&owner).await;
    let before = world.answered(&owner, &id, "Did floor 3 report?").await;
    let stored = world.messages(&id).await;
    let theirs = world.conversation(&other).await;
    let held = world
        .asked(&other, &theirs, &format!("Compare the floors {HOLD}"))
        .await;
    world.gateway.wait_calls(3).await;
    let creates = world.identity.creates();

    let reply = world.regenerate(&owner, &id).await;
    reply.expect_error(503, "unavailable", "question.busy");
    assert_eq!(reply.header("retry-after"), Some("5"));
    assert_eq!(world.messages(&id).await, stored, "nothing was discarded");
    assert_eq!(world.discards(&id).await, 0);
    assert_eq!(world.count(RUNS_OF, &id).await, 1, "no run was recorded");
    assert_eq!(world.identity.creates(), creates, "no identity call");

    world.gateway.open(1);
    world.wait_run(&other, &theirs, &held, "answered").await;
    let run = world.regenerated(&owner, &id).await;
    let after = world.wait_run(&owner, &id, &run, "answered").await;
    assert_eq!(world.discards(&id).await, 1);
    assert_eq!(
        (&after["title"], &after["title_source"]),
        (&before["title"], &before["title_source"]),
        "regenerating the first round keeps the title"
    );
    assert_eq!(after["title_source"], "provisional");
    println!("a full replica: 503 and the round in place; regenerated once a seat was free");
}

/// After the commit the discarded answer does not come back: when the identity service refuses
/// the delegation, the regeneration answers its code and ends `failed`, and the conversation
/// ends with the round before. The title stays; the next question is answered after it.
#[tokio::test(flavor = "multi_thread")]
async fn a_refused_delegation_after_the_discard_leaves_the_answer_gone() {
    let Some(world) = world().await else {
        return;
    };
    let owner = User::new(TenantId::generate(), TenantRole::Viewer);
    let id = world.conversation(&owner).await;
    world.answered(&owner, &id, "Did floor 3 report?").await;
    let before = world
        .answered(&owner, &id, "Which meter used the most?")
        .await;

    world.identity.refuse(Some(ErrorCode::Unavailable));
    let named = world.last_answer_seq(&id).await;
    let reply = world.regenerate_answer(&owner, &id, named).await;
    assert_eq!(reply.status, 503, "{}", reply.body);
    assert_eq!(reply.json()["code"], "unavailable");
    let run = world.identity.run_ids().last().cloned().expect("asked");
    let failed = world.wait_run(&owner, &id, &run, "failed").await;
    assert_eq!(failed["latest_run"]["result"], "delegation_unavailable");
    assert_eq!(failed["latest_run"]["message_key"], "question.not_started");
    assert_eq!(
        shown(&failed),
        shown(&before)[..2],
        "the discarded round does not come back"
    );
    assert_eq!(failed["title"], before["title"], "the title stays");
    assert_eq!(world.discards(&id).await, 1);

    // The client retries the same request (the 503 is `retryable`): it names the answer that
    // is gone, so the round before stays. With the identity service back it is still refused.
    let stored = world.messages(&id).await;
    let creates = world.identity.creates();
    for refusal in [Some(ErrorCode::Unavailable), None] {
        world.identity.refuse(refusal);
        world
            .regenerate_answer(&owner, &id, named)
            .await
            .expect_error(409, "conflict", "question.regenerate_stale");
    }
    assert_eq!(world.messages(&id).await, stored, "the round before stays");
    assert_eq!(world.discards(&id).await, 1);
    assert_eq!(world.identity.creates(), creates, "no identity call");
    assert_eq!(shown(&world.read(&owner, &id).await), shown(&before)[..2]);

    world.identity.refuse(None);
    let next = world.answered(&owner, &id, "And floor 4?").await;
    assert_eq!(shown(&next)[..2], shown(&before)[..2]);
    assert_eq!(shown(&next)[2], pair("question", "And floor 4?"));
    println!("a refused delegation: failed, the old answer gone, the next question answered");
}

/// A running regeneration counts toward its user's running questions, and a user at the limit
/// cannot regenerate (429), with nothing discarded.
#[tokio::test(flavor = "multi_thread")]
async fn regenerations_count_toward_the_users_running_limit() {
    let Some(world) = world_with(&[("TRUSTIOT__QUESTIONS__MAX_RUNNING_PER_USER", "1")]).await
    else {
        return;
    };
    let owner = User::new(TenantId::generate(), TenantRole::Viewer);
    let first = world.conversation(&owner).await;
    let second = world.conversation(&owner).await;
    let held = format!("Compare the floors {HOLD}");
    let run = world.asked(&owner, &first, &held).await;
    world.gateway.wait_calls(1).await;
    world.gateway.open(1);
    world.wait_run(&owner, &first, &run, "answered").await;
    world.answered(&owner, &second, "Did floor 3 report?").await;

    // The regeneration of the held question waits at the gate: it is running.
    let regeneration = world.regenerated(&owner, &first).await;
    world.gateway.wait_calls(5).await;
    world
        .ask(&owner, &second, "And floor 4?")
        .await
        .expect_error(429, "rate_limited", "question.running_limited");
    let stored = world.messages(&second).await;
    world.regenerate(&owner, &second).await.expect_error(
        429,
        "rate_limited",
        "question.running_limited",
    );
    assert_eq!(
        world.messages(&second).await,
        stored,
        "nothing was discarded"
    );
    assert_eq!(world.discards(&second).await, 0);

    world.gateway.open(1);
    world
        .wait_run(&owner, &first, &regeneration, "answered")
        .await;
    let run = world.regenerated(&owner, &second).await;
    world.wait_run(&owner, &second, &run, "answered").await;
    println!("a running regeneration counts toward the limit; at the limit, 429 and no discard");
}

/// The history limit applies to the history the regenerated answer continues: a conversation
/// whose last answer took it to 16 MiB takes no new question but regenerates that answer; one
/// whose earlier rounds hold 16 MiB refuses the regeneration (409 `question.history_full`) and
/// discards nothing.
#[tokio::test(flavor = "multi_thread")]
async fn the_history_limit_is_for_the_history_without_the_last_round() {
    let Some(world) = world().await else {
        return;
    };
    let owner = User::new(TenantId::generate(), TenantRole::Viewer);
    let full = "x".repeat(assistant::store::MAX_HISTORY_BYTES);
    let long_answer = world
        .conversation_of(
            &owner,
            &[
                (EntryKind::Question, "Did floor 3 report?"),
                (EntryKind::Answer, "Yes."),
                (EntryKind::Question, "List every reading."),
                (EntryKind::Answer, full.as_str()),
            ],
        )
        .await;
    let long_start = world
        .conversation_of(
            &owner,
            &[
                (EntryKind::Question, full.as_str()),
                (EntryKind::Answer, "Yes."),
                (EntryKind::Question, "And floor 4?"),
                (EntryKind::Answer, "Yes."),
            ],
        )
        .await;

    world
        .ask(&owner, &long_answer, "One more?")
        .await
        .expect_error(409, "conflict", "question.history_full");
    let stored = world.messages(&long_start).await;
    world.regenerate(&owner, &long_start).await.expect_error(
        409,
        "conflict",
        "question.history_full",
    );
    assert_eq!(
        world.messages(&long_start).await,
        stored,
        "nothing discarded"
    );
    assert_eq!(world.discards(&long_start).await, 0);
    assert_eq!(world.identity.creates(), 0);

    let run = world.regenerated(&owner, &long_answer).await;
    let detail = world.wait_run(&owner, &long_answer, &run, "answered").await;
    assert_eq!(
        shown(&detail),
        [
            pair("question", "Did floor 3 report?"),
            pair("answer", "Yes."),
            pair("question", "List every reading."),
            pair("answer", &answer_text(1)),
        ]
    );
    println!("the limit is for the history without the last round");
}

/// Regenerating stays in the caller's tenant: the same person in another tenant gets the one
/// 404 for a conversation of theirs in tenant B, which stays as it was; in tenant B the same
/// request regenerates. `tests/cross_tenant.rs` exempts the endpoint and points here.
#[tokio::test(flavor = "multi_thread")]
async fn regenerations_stay_in_their_tenant() {
    let Some(world) = world().await else {
        return;
    };
    let in_b = User::new(TenantId::generate(), TenantRole::Admin);
    let in_a = User {
        tenant: TenantId::generate(),
        ..in_b
    };
    let conversation_b = world.conversation(&in_b).await;
    world
        .answered(&in_b, &conversation_b, "Did floor 3 report?")
        .await;
    let stored = world.messages(&conversation_b).await;
    let creates = world.identity.creates();

    let reply = world.regenerate(&in_a, &conversation_b).await;
    let missing = world.regenerate(&in_a, &Uuid::now_v7().to_string()).await;
    assert_eq!(reply.status, 404, "{}", reply.body);
    assert_eq!(seen(&reply), seen(&missing));
    assert_eq!(world.messages(&conversation_b).await, stored);
    assert_eq!(world.discards(&conversation_b).await, 0);
    assert_eq!(world.identity.creates(), creates);

    let run = world.regenerated(&in_b, &conversation_b).await;
    world
        .wait_run(&in_b, &conversation_b, &run, "answered")
        .await;
    println!("tenant A reached nothing of tenant B's conversation; its owner there regenerated");
}

/// Security review M-1 (b): two requests naming the same answer, sent at once (a double submit,
/// or a retry racing the first), discard one round between them: exactly one is accepted, the
/// other is refused with 409 (`conversation.question_running` while the first runs, or
/// `question.regenerate_stale` once it answered), and no round before is discarded. Five times
/// over, on the same conversation, with nothing held at the gate.
#[tokio::test(flavor = "multi_thread")]
async fn two_requests_naming_the_same_answer_discard_one_round() {
    let Some(world) = world().await else {
        return;
    };
    let owner = User::new(TenantId::generate(), TenantRole::Viewer);
    let id = world.conversation(&owner).await;
    world.answered(&owner, &id, "Did floor 3 report?").await;
    world
        .answered(&owner, &id, "Which meter used the most?")
        .await;
    let first_round = world.messages(&id).await[..ROUND].to_vec();
    let mut refusals = Vec::new();
    for attempt in 1..=5_i64 {
        let named = world.last_answer_seq(&id).await;
        let creates = world.identity.creates();
        let (one, other) = tokio::join!(
            world.regenerate_answer(&owner, &id, named),
            world.regenerate_answer(&owner, &id, named),
        );
        let (accepted, refused) = match (one.status, other.status) {
            (202, _) => (one, other),
            (_, 202) => (other, one),
            _ => panic!(
                "none accepted: {} {} / {} {}",
                one.status, one.body, other.status, other.body
            ),
        };
        assert_eq!(refused.status, 409, "{}", refused.body);
        let key = refused.json()["message_key"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        assert!(
            ["conversation.question_running", "question.regenerate_stale"].contains(&key.as_str()),
            "{key}"
        );
        refusals.push(key);
        world
            .wait_run(&owner, &id, &run_id(&accepted), "answered")
            .await;
        assert_eq!(world.discards(&id).await, attempt, "one round a time");
        assert_eq!(world.identity.creates(), creates + 1, "one delegation");
        assert_eq!(
            world.messages(&id).await[..ROUND],
            first_round[..],
            "the first round stays"
        );
        assert_eq!(
            world.read(&owner, &id).await["messages"]
                .as_array()
                .map(Vec::len),
            Some(4)
        );
    }
    println!(
        "two requests naming one answer, five times: one accepted each time; refused with {refusals:?}"
    );
}

/// Security review L-3 (2): a last round whose question is not stored in the form the service
/// writes a question in (here a text block, and the string form with other spacing) is not
/// regenerated: 409 `question.nothing_to_regenerate`, nothing discarded, no identity call.
#[tokio::test(flavor = "multi_thread")]
async fn a_question_not_in_the_services_form_is_not_regenerated() {
    let Some(world) = world().await else {
        return;
    };
    let owner = User::new(TenantId::generate(), TenantRole::Viewer);
    for question in [
        r#"{"role":"user","content":[{"type":"text","text":"Did floor 3 report?"}]}"#,
        r#"{"role": "user", "content": "Did floor 3 report?"}"#,
    ] {
        let id = world.conversation(&owner).await;
        let mut admin = world.superuser().await;
        sqlx::query(STORE_ROUND)
            .bind(&id)
            .bind(question)
            .bind(r#"{"role":"assistant","content":[{"type":"text","text":"Yes."}]}"#)
            .execute(&mut admin)
            .await
            .expect("store a round as the superuser");
        let stored = world.messages(&id).await;
        assert_eq!(world.last_answer_seq(&id).await, 1);
        world.regenerate(&owner, &id).await.expect_error(
            409,
            "conflict",
            "question.nothing_to_regenerate",
        );
        assert_eq!(world.messages(&id).await, stored, "{question}");
        assert_eq!(world.discards(&id).await, 0);
        assert_eq!(world.count(RUNS_OF, &id).await, 0);
    }
    assert_eq!(world.identity.creates(), 0, "no identity call");
    assert!(world.gateway.requests().is_empty(), "no model call");
    println!("a question not in the service's form: 409 nothing_to_regenerate, nothing discarded");
}

/// Security review L-3 (3): a conversation stays hidden while questions run. A colleague in the
/// tenant, and the owner in another tenant, get the one 404 (byte for byte as for an unknown
/// conversation), not a 409 or a 429, while the owner's question runs in it and while the
/// colleague is at their own running limit.
#[tokio::test(flavor = "multi_thread")]
async fn a_conversation_stays_hidden_while_questions_run() {
    let Some(world) = world_with(&[("TRUSTIOT__QUESTIONS__MAX_RUNNING_PER_USER", "1")]).await
    else {
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
    world.answered(&owner, &id, "Did floor 3 report?").await;
    let theirs = world.conversation(&colleague).await;
    let named = world.last_answer_seq(&id).await;
    let held = format!("Compare the floors {HOLD}");
    let running = world.asked(&owner, &id, &held).await;
    let colleagues = world.asked(&colleague, &theirs, &held).await;
    world.gateway.wait_calls(4).await;
    world
        .ask(
            &colleague,
            &world.conversation(&colleague).await,
            "And floor 4?",
        )
        .await
        .expect_error(429, "rate_limited", "question.running_limited");
    let stored = world.messages(&id).await;

    let unknown = Uuid::now_v7().to_string();
    for (who, what) in [
        (&colleague, "a colleague at their limit"),
        (&elsewhere, "the owner in another tenant"),
    ] {
        let missing = world.regenerate_answer(who, &unknown, named).await;
        assert_eq!(missing.status, 404, "{what}: {}", missing.body);
        let reply = world.regenerate_answer(who, &id, named).await;
        assert_eq!(seen(&reply), seen(&missing), "{what}");
    }
    world
        .regenerate_answer(&owner, &id, named)
        .await
        .expect_error(409, "conflict", "conversation.question_running");
    assert_eq!(world.messages(&id).await, stored, "nothing discarded");
    assert_eq!(world.discards(&id).await, 0);

    world.gateway.open(2);
    world.wait_run(&owner, &id, &running, "answered").await;
    world
        .wait_run(&colleague, &theirs, &colleagues, "answered")
        .await;
    println!("while questions ran: the one 404 for a colleague and for another tenant");
}

/// Security review L-1: a regeneration is a start, counted with the conversations a user starts
/// against the daily limit (`store::MAX_STARTS_PER_DAY`), so regenerating in a loop cannot store
/// rounds without bound. At the limit both a new conversation and a regeneration get 429
/// `conversation.start_limited` with `Retry-After`; the regeneration discards nothing.
#[tokio::test(flavor = "multi_thread")]
async fn regenerations_count_toward_the_daily_starts() {
    let Some(world) = world().await else {
        return;
    };
    let owner = User::new(TenantId::generate(), TenantRole::Viewer);
    let id = world.conversation(&owner).await;
    world.answered(&owner, &id, "Did floor 3 report?").await;
    // The day's starts but two, an hour ago: with this conversation, one start is left.
    let mut admin = world.superuser().await;
    sqlx::query(SEED_STARTS)
        .bind(owner.tenant.to_string())
        .bind(owner.id.to_string())
        .bind(i32::try_from(assistant::store::MAX_STARTS_PER_DAY - 2).expect("a small limit"))
        .bind(60_i32)
        .execute(&mut admin)
        .await
        .expect("store the day's starts");

    let run = world.regenerated(&owner, &id).await;
    world.wait_run(&owner, &id, &run, "answered").await;
    assert_eq!(world.discards(&id).await, 1);

    let started = world
        .service
        .as_user(
            &world.issuer,
            &owner,
            Method::POST,
            CONVERSATIONS_PATH,
            Some(&json!({})),
        )
        .await;
    started.expect_error(429, "rate_limited", "conversation.start_limited");
    assert!(started.header("retry-after").is_some());
    let stored = world.messages(&id).await;
    let creates = world.identity.creates();
    let refused = world.regenerate(&owner, &id).await;
    refused.expect_error(429, "rate_limited", "conversation.start_limited");
    assert_eq!(
        refused.json()["details"]["max"],
        assistant::store::MAX_STARTS_PER_DAY
    );
    let retry_after: u64 = refused
        .header("retry-after")
        .and_then(|value| value.parse().ok())
        .expect("a Retry-After in seconds");
    // The oldest start was an hour ago, so it leaves the 24 hours within 23.
    assert!(
        (22 * 3600..=23 * 3600).contains(&retry_after),
        "{retry_after}"
    );
    assert_eq!(world.messages(&id).await, stored, "nothing discarded");
    assert_eq!(world.discards(&id).await, 1);
    assert_eq!(world.identity.creates(), creates, "no identity call");
    println!("the 100th start was a regeneration; then both starting and regenerating got 429");
}

/// The switch that gives new conversations the answer tool (`streams.answer_tool`); on by
/// default, so a test of text answers sets it to `false`.
const ANSWER_TOOL_SWITCH: &str = "TRUSTIOT__STREAMS__ANSWER_TOOL";

/// The paragraphs of the regenerated answer, written with the answer tool.
const STREAMED_FIRST: &str = "Floor 3 used 412 kWh last week, 8 % more than the week before.";
const STREAMED_SECOND: &str = "Nothing else changed.";

/// The partial answer of the run `$1`, if one is stored.
const PARTIAL_OF: &str = "SELECT text FROM assistant.answer_partials WHERE run_id = $1::uuid";

/// One server-sent event of an answer stream: its name and its data as JSON.
struct StreamEvent {
    event: String,
    data: Value,
}

/// An open answer stream, read event by event.
struct AnswerStream {
    response: reqwest::Response,
    buffer: String,
}

impl AnswerStream {
    /// The next event that is not a heartbeat; `None` once the server closed the stream.
    async fn next(&mut self) -> Option<StreamEvent> {
        loop {
            while let Some(end) = self.buffer.find("\n\n") {
                let frame: String = self.buffer.drain(..end + 2).collect();
                let mut event = String::new();
                let mut data = Value::Null;
                for line in frame.lines() {
                    if let Some(name) = line.strip_prefix("event: ") {
                        name.clone_into(&mut event);
                    } else if let Some(text) = line.strip_prefix("data: ") {
                        data = serde_json::from_str(text).expect("an event's data is JSON");
                    }
                }
                if event != "heartbeat" {
                    return Some(StreamEvent { event, data });
                }
            }
            let chunk = tokio::time::timeout(WAIT, self.response.chunk())
                .await
                .expect("the next piece of the stream within the bound");
            match chunk {
                Ok(Some(chunk)) => self
                    .buffer
                    .push_str(std::str::from_utf8(&chunk).expect("the stream is UTF-8")),
                Ok(None) | Err(_) => return None,
            }
        }
    }

    /// The text of the `text` events from here until the text is `expected`, each starting
    /// where the last one ended (from `offset`).
    async fn text_until(&mut self, offset: u64, expected: &str) {
        let mut text = String::new();
        let mut at = offset;
        while text != expected {
            let event = self.next().await.expect("text before the stream closes");
            assert_eq!(event.event, "text", "{:?}", event.data);
            assert_eq!(
                event.data["offset"].as_u64(),
                Some(at),
                "no gap and no overlap"
            );
            let piece = event.data["text"].as_str().expect("text");
            at += u64::try_from(piece.encode_utf16().count()).expect("a length");
            text.push_str(piece);
            assert!(
                expected.starts_with(&text),
                "{text:?} does not start {expected:?}"
            );
        }
    }
}

impl World {
    /// Opens the answer stream of `run` as `user`.
    async fn stream(&self, user: &User, id: &str, run: &str) -> AnswerStream {
        let path = STREAM_PATH
            .replace("{conversation_id}", id)
            .replace("{run_id}", run);
        let request = reqwest::Client::new()
            .get(format!("{}{path}", self.service.base_url))
            .header("x-internal-identity", user.token(&self.issuer));
        let response = tokio::time::timeout(WAIT, request.send())
            .await
            .expect("the stream's head within the bound")
            .expect("open the stream");
        assert_eq!(response.status().as_u16(), 200, "the stream opens");
        AnswerStream {
            response,
            buffer: String::new(),
        }
    }

    /// The partial answer the run `run` has stored, if any.
    async fn partial(&self, run: &str) -> Option<String> {
        let mut admin = self.superuser().await;
        sqlx::query_scalar(PARTIAL_OF)
            .bind(run)
            .fetch_optional(&mut admin)
            .await
            .expect("read the partial answer")
    }
}

/// A regenerated answer goes the way of every question's answer (Laysi/trustiot-v2#147 joined
/// with Laysi/trustiot-v2#649): in a conversation started with the answer tool, the paragraph the
/// model finished in the tool is stored as the regeneration's partial answer and goes out on its
/// answer stream while the run is still `running` and before any answer is stored; once it is
/// answered the stream goes on into the stored answer, ends `answered`, and the run leaves no
/// partial answer behind.
#[tokio::test(flavor = "multi_thread")]
async fn a_regenerated_answer_streams_while_it_runs_like_a_question() {
    let Some(world) = world_with(&[(ANSWER_TOOL_SWITCH, "true")]).await else {
        return;
    };
    let owner = User::new(TenantId::generate(), TenantRole::Viewer);
    let id = world.conversation(&owner).await;
    world
        .answered(&owner, &id, "How much did floor 3 use?")
        .await;

    // The regeneration's model call writes its answer with the answer tool, in pieces the test
    // sends: through the end of the first paragraph, then the rest.
    let (sender, reply) = support::events_reply();
    world.gateway.queue(reply);
    let text = format!("{STREAMED_FIRST}\n\n{STREAMED_SECOND}");
    let pieces = support::json_pieces(&text, 7);
    let events = support::turn_events(
        &[
            support::thinking_block(),
            support::call_block(
                "toolu_regenerated",
                ANSWER_TOOL,
                &pieces.iter().map(String::as_str).collect::<Vec<_>>(),
            ),
        ],
        "tool_use",
    );
    let mut written = String::new();
    let through = pieces
        .iter()
        .position(|piece| {
            written.push_str(piece);
            written.contains("\\n\\nNothing")
        })
        .expect("the second paragraph starts in a piece");
    // The message's start, the thinking (start, two deltas, stop), the call's start, then the
    // pieces through the one that starts the second paragraph.
    let cut = 1 + 4 + 1 + through + 1;
    assert!(cut < events.len());

    let run = world.regenerated(&owner, &id).await;
    for event in &events[..cut] {
        sender.send(event.clone()).await.expect("the gateway reads");
    }
    let mut stream = world.stream(&owner, &id, &run).await;
    stream.text_until(0, STREAMED_FIRST).await;
    let running = world.read(&owner, &id).await;
    assert_eq!(running["latest_run"]["id"], run.as_str());
    assert_eq!(running["latest_run"]["status"], "running");
    assert!(
        shown(&running).iter().all(|(kind, _)| kind != "answer"),
        "no answer is stored while the regeneration runs"
    );
    assert_eq!(
        world.partial(&run).await.as_deref(),
        Some(STREAMED_FIRST),
        "the regeneration's partial answer is the paragraph that went out"
    );

    for event in &events[cut..] {
        sender.send(event.clone()).await.expect("the gateway reads");
    }
    drop(sender);
    let answered = world.wait_run(&owner, &id, &run, "answered").await;
    let stored = last_answer(&answered);
    assert!(stored.starts_with(STREAMED_FIRST), "{stored:?}");
    assert!(stored.ends_with(STREAMED_SECOND), "{stored:?}");
    let rest = stored
        .strip_prefix(STREAMED_FIRST)
        .expect("the stored answer starts with what went out");
    let offset = u64::try_from(STREAMED_FIRST.encode_utf16().count()).expect("a length");
    stream.text_until(offset, rest).await;
    let end = stream.next().await.expect("the end");
    assert_eq!(end.event, "end");
    assert_eq!(end.data["status"], "answered");
    assert!(
        stream.next().await.is_none(),
        "the stream closes after its end"
    );
    assert_eq!(world.partial(&run).await, None, "no partial answer is left");
    assert_eq!(world.discards(&id).await, 1);
    println!(
        "a regeneration's first paragraph went out while it ran, from its partial answer; then \
         the stored answer and `answered`"
    );
}
