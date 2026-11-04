//! The answer stream (Laysi/trustiot-v2#147) once it holds an answer: reading a stored answer
//! loads its whole message, so a stream first reserves [`ANSWER_READ_FACTOR`] times the
//! message's length in the replica's result memory (`dashboards::memory`), holds only the text's
//! length once it is read, and gives it back when it closes; and a reader too slow to take the
//! answer before its conversation is deleted gets no more of it. The stream's routes run in this
//! process, so a reader that stops reading stops the stream after the few frames it queues; the
//! memory is a small budget the test holds or releases; the questions are stored with the
//! service role, as the question API stores them.
//!
//! The stream end to end, through the service binary, is tests/answer_stream.rs.
//!
//! Starts a throwaway Postgres with TLS (podman or docker); skipped without either unless `CI`
//! is set.

mod support;

use std::sync::Arc;
use std::time::{Duration, Instant};

use assistant::Agent;
use assistant::api::answer_stream::{
    self, ANSWER_MEMORY_WAIT, ANSWER_READ_FACTOR, BUSY_RETRY_AFTER, STREAM_PATH, Streams,
};
use assistant::dashboards::memory::ResultMemory;
use assistant::history::{Conversation, Principal};
use assistant::questions::{CANCEL_POLL_INTERVAL, Questions, Settings as QuestionSettings};
use assistant::store::{self, StartedQuestion, Stored};
use assistant::tools::NoDelegation;
use axum::Router;
use axum::body::Body;
use axum::http::header::RETRY_AFTER;
use axum::http::{Request, StatusCode};
use contract::identity::{TenantGrant, TenantRole};
use contract::{INTERNAL_IDENTITY_HEADER, InternalClaims, ServiceName, TenantId};
use futures_util::StreamExt;
use futures_util::stream::BoxStream;
use serde_json::{Value, json};
use service_kit::identity::UserIdentity;
use service_kit::tenant::{TenantDb, TenantTransaction};
use sqlx::postgres::PgPoolOptions;
use support::{StandIn, WAIT, agent, fixture, ok, respond, settings};
use testkit::identity::TestIssuer;
use testkit::product::{DATABASE, ProductPostgres, product_postgres};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;
use uuid::Uuid;

/// The heartbeat interval of the streams here: short, so a stream that waits shows it.
const HEARTBEAT: Duration = Duration::from_millis(100);

/// The question each run asks.
const QUESTION: &str = "How much energy did floor 3 use?";

/// The stand-in model's answer: many text events' worth, more than a stream queues for a reader
/// that stopped reading.
fn long_answer() -> String {
    let paragraph = "All twelve meters on floor 3 reported in the last hour; use was normal. ";
    let mut answer = String::new();
    while answer.len() < 20 * answer_stream::TEXT_CHUNK_UNITS {
        answer.push_str(paragraph);
    }
    answer
}

/// The recorded answer turn with [`long_answer`] as its text.
fn answer_turn() -> String {
    let mut message: Value =
        serde_json::from_str(&fixture("answer_end_turn.json")).expect("the fixture is JSON");
    message["content"] = json!([{"type": "text", "text": long_answer()}]);
    message.to_string()
}

struct World {
    product: Arc<ProductPostgres>,
    db: TenantDb,
    issuer: TestIssuer,
    agent: Arc<Agent>,
    _stand_in: StandIn,
}

async fn world() -> Option<World> {
    let product = product_postgres(&["assistant_app"]).await?;
    let db = TenantDb::connect_lazy(
        product.postgres.options("assistant_app", DATABASE),
        PgPoolOptions::new().max_connections(4),
    )
    .expect("the service role's pool verifies the server");
    // No tools: every question is answered at once.
    let stand_in = StandIn::start(respond(|_request, _index| ok(answer_turn()))).await;
    let agent = Arc::new(agent(&stand_in, Vec::new(), settings()));
    Some(World {
        product,
        db,
        issuer: TestIssuer::new(),
        agent,
        _stand_in: stand_in,
    })
}

/// A user of a tenant: the claims its tokens carry and its verified identity.
struct User {
    claims: InternalClaims,
    identity: UserIdentity,
}

impl User {
    fn principal(&self) -> Principal {
        Principal {
            tenant_id: self
                .identity
                .tenant_id()
                .expect("the user acts in a tenant"),
            user_id: self.identity.user_id(),
        }
    }
}

impl World {
    fn user(&self, tenant: TenantId) -> User {
        let claims = TestIssuer::claims(
            ServiceName::Assistant,
            Some(TenantGrant {
                id: tenant,
                role: TenantRole::Viewer,
            }),
        );
        let identity = self.issuer.user(ServiceName::Assistant, &claims);
        User { claims, identity }
    }

    async fn tx(&self, user: &User) -> TenantTransaction {
        self.db
            .begin(&user.identity)
            .await
            .expect("a tenant transaction")
    }

    /// A stored conversation of `user`, with no questions yet.
    async fn conversation(&self, user: &User) -> Uuid {
        let conversation = self.agent.start_conversation(user.principal(), "");
        let mut tx = self.tx(user).await;
        store::create(&mut tx, &conversation)
            .await
            .expect("store the conversation");
        tx.commit().await.expect("commit");
        conversation.id()
    }

    /// Starts a question in `id` as the question API does, commits it, and answers it in
    /// memory: the conversation with the answered round, where it was loaded, and the run.
    async fn answered_in_memory(&self, user: &User, id: Uuid) -> (Conversation, Stored, Uuid) {
        let mut tx = self.tx(user).await;
        let admitted = store::admit_question(&mut tx, user.principal(), id, 3)
            .await
            .expect("let the question through");
        let StartedQuestion { loaded, run } =
            store::start_question(&mut tx, admitted, Uuid::now_v7(), Duration::from_secs(600))
                .await
                .expect("start the question");
        tx.commit().await.expect("commit");
        let mut conversation = loaded.conversation;
        self.agent
            .ask(&mut conversation, QUESTION, Arc::new(NoDelegation))
            .await
            .expect("an answer");
        (conversation, loaded.stored, run.run_id)
    }

    /// Stores the answered round and ends the run `answered`.
    async fn finish(&self, user: &User, conversation: &Conversation, stored: Stored, run: Uuid) {
        let mut tx = self.tx(user).await;
        store::finish_answered(&mut tx, conversation, stored, run, None)
            .await
            .expect("store the answered round");
        tx.commit().await.expect("commit");
    }

    /// The stream's routes in this process, reading answers in `memory`.
    fn routes(&self, memory: &ResultMemory) -> Router {
        let questions = Questions::new(
            self.db.clone(),
            Arc::clone(&self.agent),
            None,
            QuestionSettings {
                max_chars: 16_000,
                max_running_per_user: 3,
                max_running_per_replica: 8,
                memory: 40 << 20,
                run_lifetime: Duration::from_secs(900),
            },
            CancellationToken::new(),
        );
        answer_stream::routes(
            self.issuer.verifier(ServiceName::Assistant),
            self.db.clone(),
            questions,
            Streams::new(answer_stream::Settings {
                heartbeat_interval: HEARTBEAT,
                max_per_replica: 64,
                max_per_user: 8,
                max_per_tenant: 32,
            }),
            memory.clone(),
        )
    }

    /// Opens the stream of `run` in `conversation` as `user`.
    async fn open(
        &self,
        routes: &Router,
        user: &User,
        conversation: Uuid,
        run: Uuid,
    ) -> axum::response::Response {
        let path = STREAM_PATH
            .replace("{conversation_id}", &conversation.to_string())
            .replace("{run_id}", &run.to_string());
        let request = Request::get(path)
            .header(INTERNAL_IDENTITY_HEADER, self.issuer.token(&user.claims))
            .body(Body::empty())
            .expect("a request");
        tokio::time::timeout(WAIT, routes.clone().oneshot(request))
            .await
            .expect("the stream answers")
            .expect("infallible")
    }
}

/// The stored message of `run`'s answer, in bytes, as the superuser reads it.
async fn answer_bytes(world_db: &ProductPostgres, run: Uuid) -> usize {
    let mut admin = world_db.superuser().await;
    let bytes: i64 = sqlx::query_scalar(
        "SELECT octet_length(m.body)::bigint FROM assistant.runs r
         JOIN assistant.messages m ON m.tenant_id = r.tenant_id
             AND m.conversation_id = r.conversation_id AND m.seq = r.answer_seq
         WHERE r.run_id = $1::uuid",
    )
    .bind(run.to_string())
    .fetch_one(&mut admin)
    .await
    .expect("read the answer's length");
    usize::try_from(bytes).expect("a length")
}

/// A stream's events as they come: their names and data.
struct Events {
    frames: BoxStream<'static, Result<axum::body::Bytes, axum::Error>>,
}

impl Events {
    fn of(response: axum::response::Response) -> Self {
        assert_eq!(response.status(), StatusCode::OK);
        Self {
            frames: response.into_body().into_data_stream().boxed(),
        }
    }

    /// The next event (each frame of the stream is one); `None` once the stream closed.
    async fn next(&mut self) -> Option<(String, Value)> {
        let frame = tokio::time::timeout(WAIT, self.frames.next())
            .await
            .expect("the stream's next frame")?
            .expect("a frame");
        let text = std::str::from_utf8(&frame).expect("UTF-8");
        let field = |name: &str| {
            text.lines()
                .find_map(|line| line.strip_prefix(name))
                .map(str::to_owned)
        };
        let event = field("event: ").expect("an event name");
        let data = serde_json::from_str(&field("data: ").expect("data")).expect("JSON data");
        Some((event, data))
    }
}

/// Waits until `memory` holds nothing.
async fn wait_until_released(memory: &ResultMemory, what: &str) {
    let started = Instant::now();
    while memory.used() != 0 {
        assert!(
            started.elapsed() < WAIT,
            "{what}: the memory was not given back within {WAIT:?} ({} bytes held)",
            memory.used()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// A stream waits for memory to read its answer and sends nothing of it meanwhile, then reads it
/// once there is room; it holds only the text's length while it sends, and nothing after. Opening
/// a stream of an answered question with no room waits [`ANSWER_MEMORY_WAIT`] and answers 503
/// `stream.busy` with `Retry-After`, holding nothing.
#[tokio::test(flavor = "multi_thread")]
async fn an_answer_is_read_in_memory_reserved_first() {
    let Some(world) = world().await else {
        return;
    };
    let owner = world.user(TenantId::generate());
    let other = TenantId::generate();
    let id = world.conversation(&owner).await;
    // Room for one answer of the stand-in's size; another tenant holds it all.
    let budget = 4 << 20;
    let memory = ResultMemory::new(budget, budget);
    let routes = world.routes(&memory);

    // A stream of a running question; the question answers while the memory is held.
    let (conversation, stored, run) = world.answered_in_memory(&owner, id).await;
    let held = memory
        .reserve(other, budget, Duration::ZERO)
        .await
        .expect("the whole budget");
    let mut events = Events::of(world.open(&routes, &owner, id, run).await);
    assert_eq!(events.next().await.expect("the first event").0, "heartbeat");
    world.finish(&owner, &conversation, stored, run).await;
    let stored_bytes = answer_bytes(&world.product, run).await;
    assert!(
        stored_bytes * ANSWER_READ_FACTOR <= budget,
        "the test's budget fits the answer"
    );
    // Several polls' worth of heartbeats: the stream learns the question answered (it polls,
    // since this process did not run it) but has no memory to read the answer.
    let waited_from = Instant::now();
    let mut heartbeats = 0;
    while waited_from.elapsed() < 3 * CANCEL_POLL_INTERVAL {
        let (event, _) = events.next().await.expect("the stream stays open");
        assert_eq!(event, "heartbeat", "nothing of the answer without memory");
        heartbeats += 1;
    }
    assert_eq!(
        memory.used(),
        budget,
        "the stream reserved nothing while it waited"
    );
    drop(held);
    let mut text = String::new();
    let mut most_held = 0;
    let end = loop {
        match events.next().await.expect("the answer and the end") {
            (event, _) if event == "heartbeat" => {}
            (event, data) if event == "text" => {
                most_held = most_held.max(memory.used());
                text.push_str(data["text"].as_str().expect("text"));
            }
            (event, data) if event == "end" => break data,
            (event, data) => panic!("unexpected {event}: {data}"),
        }
    };
    assert_eq!(end["status"], "answered");
    assert!(!text.is_empty(), "the answer's text");
    assert!(
        most_held <= text.len(),
        "while it sends, the stream holds no more than the text ({most_held} bytes)"
    );
    assert!(
        events.next().await.is_none(),
        "the stream closes after its end"
    );
    wait_until_released(&memory, "a closed stream").await;
    println!(
        "answer message {stored_bytes} bytes, text {} bytes; {heartbeats} heartbeats while the \
         memory was held",
        text.len()
    );

    // Opening a stream of the answered question while the memory is held: 503 after the wait.
    let held = memory
        .reserve(other, budget, Duration::ZERO)
        .await
        .expect("the whole budget");
    let started = Instant::now();
    let refused = world.open(&routes, &owner, id, run).await;
    assert!(
        started.elapsed() >= ANSWER_MEMORY_WAIT,
        "it waited for the memory first"
    );
    assert_eq!(refused.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        refused
            .headers()
            .get(RETRY_AFTER)
            .and_then(|value| value.to_str().ok()),
        Some(BUSY_RETRY_AFTER.as_secs().to_string().as_str())
    );
    let body = axum::body::to_bytes(refused.into_body(), 1 << 16)
        .await
        .expect("the body");
    let body: Value = serde_json::from_slice(&body).expect("JSON");
    assert_eq!(body["code"], "unavailable");
    assert_eq!(body["message_key"], "stream.busy");
    assert_eq!(memory.used(), budget, "the refused stream holds nothing");
    drop(held);

    // With room, it opens at once and gets the same text.
    let mut events = Events::of(world.open(&routes, &owner, id, run).await);
    let mut again = String::new();
    loop {
        match events.next().await.expect("the answer and the end") {
            (event, data) if event == "text" => {
                again.push_str(data["text"].as_str().expect("text"));
            }
            (event, _) if event == "end" => break,
            _ => {}
        }
    }
    assert_eq!(again, text);
    wait_until_released(&memory, "a stream of an answered question").await;
}

/// A reader that stops reading while the answer goes out, and the conversation is deleted: the
/// stream sends at most the few frames it had queued, then closes without the rest and without
/// an `end` once it reads the run again ([`CANCEL_POLL_INTERVAL`] into sending), and gives its
/// memory back.
#[tokio::test(flavor = "multi_thread")]
async fn a_deletion_stops_an_answer_a_slow_reader_has_not_taken() {
    let Some(world) = world().await else {
        return;
    };
    let owner = world.user(TenantId::generate());
    let id = world.conversation(&owner).await;
    let (conversation, stored, run) = world.answered_in_memory(&owner, id).await;
    world.finish(&owner, &conversation, stored, run).await;
    let memory = ResultMemory::new(16 << 20, 16 << 20);
    let routes = world.routes(&memory);

    let mut events = Events::of(world.open(&routes, &owner, id, run).await);
    assert_eq!(events.next().await.expect("the first event").0, "heartbeat");
    let (event, first) = events.next().await.expect("the first piece");
    assert_eq!(event, "text");
    let mut text = first["text"].as_str().expect("text").to_owned();
    // The reader stops; the conversation is deleted; time passes beyond a re-read.
    let mut tx = world.tx(&owner).await;
    store::delete(&mut tx, owner.principal(), id)
        .await
        .expect("delete the conversation");
    tx.commit().await.expect("commit");
    tokio::time::sleep(2 * CANCEL_POLL_INTERVAL).await;
    let mut after = 0;
    while let Some((event, data)) = events.next().await {
        assert_eq!(event, "text", "no end after the deletion: {data}");
        text.push_str(data["text"].as_str().expect("text"));
        after += 1;
    }
    let whole = long_answer();
    assert!(
        text.len() < whole.len() && whole.starts_with(&text),
        "the stream stopped partway ({} of {} bytes)",
        text.len(),
        whole.len()
    );
    wait_until_released(&memory, "a stream closed by a deletion").await;
    println!(
        "after the deletion the stream sent {after} queued pieces, {} of {} bytes in all, and closed",
        text.len(),
        whole.len()
    );
}
