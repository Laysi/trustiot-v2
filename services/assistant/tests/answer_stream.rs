//! The answer stream (Laysi/trustiot-v2#147) end to end: the service binary
//! (`tests/service/mod.rs`) on the product's schema, a stand-in AI gateway whose model calls for a
//! question with [`HOLD`] wait at a gate the test opens, and a client that reads the server-sent
//! events as they come, over a plain connection or through a stand-in proxy that cuts a
//! connection on which nothing passed for a configured time, as Cloudflare does after 125
//! seconds (docs/feasibility/2026-10-10-edge-poc.md).
//!
//! Without `identity.exchange_url` the questions run without a delegation (no tools need one
//! here), so no stand-in identity service is needed.
//!
//! Starts a throwaway Postgres with TLS (podman or docker); skipped without either unless `CI`
//! is set.

mod service;
mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use assistant::answer::ANSWER_TOOL;
use assistant::api::answer_stream::{STREAM_PATH, TEXT_CHUNK_UNITS};
use assistant::api::conversations::{CONVERSATION_PATH, CONVERSATIONS_PATH};
use assistant::api::questions::{QUESTIONS_PATH, STOP_PATH};
use assistant::api::sharing::SHARE_PATH;
use contract::identity::TenantRole;
use contract::{ServiceName, TenantId};
use reqwest::Method;
use serde_json::{Value, json};
use service::{Reply, Service, User};
use support::{BoxFuture, Recorded, Responder, StandIn, WAIT, fixture, ok};
use testkit::identity::TestIssuer;
use testkit::product::{ProductPostgres, product_postgres};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use tokio::time::Instant;
use uuid::Uuid;

/// A question that contains this waits at the gate for each of its model calls.
const HOLD: &str = "[hold]";

/// The heartbeat interval of the services that go through the idle-cutting proxy: far shorter
/// than [`PROXY_IDLE`].
const FAST_HEARTBEAT: &str = "200ms";

/// How long the stand-in proxy lets a connection carry nothing before it cuts it: Cloudflare's
/// 125 seconds, scaled down.
const PROXY_IDLE: Duration = Duration::from_secs(2);

/// How many heartbeats a stream through the proxy gets before its question may answer: their
/// gaps add up to several times [`PROXY_IDLE`] at [`FAST_HEARTBEAT`].
const HEARTBEATS_PAST_THE_IDLE_CUT: usize = 30;

/// Put in the thinking, the text and the tool input of a model turn that calls a tool: an
/// intermediate step, which the stream must never carry.
const STEP_MARKER: &str = "step-narration-4c1d";
const STEP_THINKING_MARKER: &str = "step-thinking-77ae";
const TOOL_INPUT_MARKER: &str = "tool-input-0b9e";
/// Put in the answer's thinking.
const ANSWER_THINKING_MARKER: &str = "answer-thinking-a3f0";
/// Put in the SQL of the answer's code fence, which the answer guard removes.
const SQL_MARKER: &str = "meter_secret_table";

/// The answer of the long-answer fixture: more than two text events' worth, with characters of
/// two and four bytes, and one outside the Basic Multilingual Plane (two UTF-16 code units).
fn long_answer() -> String {
    let paragraph = "第三層的十二個電表在過去一小時都有回報，用電量正常。 ";
    let mut answer = String::from("Summary 😀 ");
    while answer.encode_utf16().count() < TEXT_CHUNK_UNITS * 2 + 100 {
        answer.push_str(paragraph);
    }
    answer.push_str("End.");
    answer
}

/// A recorded answer turn whose text is `text` (its thinking carries [`ANSWER_THINKING_MARKER`]).
fn answer_with(text: &str) -> String {
    let mut message: Value =
        serde_json::from_str(&fixture("answer_end_turn.json")).expect("the fixture is JSON");
    message["content"] = json!([
        {"type": "thinking", "thinking": ANSWER_THINKING_MARKER, "signature": "EuYBCkYICxIMfinal"},
        {"type": "text", "text": text},
    ]);
    message.to_string()
}

/// A model turn that writes text, then calls a tool: an intermediate step.
fn narrated_step() -> String {
    let mut message: Value =
        serde_json::from_str(&support::tool_step(1)).expect("the fixture is JSON");
    message["content"] = json!([
        {"type": "thinking", "thinking": STEP_THINKING_MARKER, "signature": "Es8CCkYICxIMstep"},
        {"type": "text", "text": format!("Let me look that up first ({STEP_MARKER}).")},
        {"type": "tool_use", "id": "toolu_step_1", "name": "count_readings",
            "input": {"meter": TOOL_INPUT_MARKER}},
    ]);
    message.to_string()
}

/// The stand-in AI gateway and the gate its held calls wait at.
struct Gateway {
    stand_in: StandIn,
    gate: Arc<Semaphore>,
}

impl Gateway {
    /// A gateway that answers the `index`th call with `answer`; a call for a question with
    /// [`HOLD`] waits for a permit of the gate first.
    async fn start(
        answer: impl Fn(&Recorded, usize) -> support::Reply + Send + Sync + 'static,
    ) -> Self {
        let gate = Arc::new(Semaphore::new(0));
        let held = Arc::clone(&gate);
        let answer = Arc::new(answer);
        let responder: Responder = Arc::new(
            move |request: Recorded, index: usize| -> BoxFuture<support::Reply> {
                let (gate, answer) = (Arc::clone(&held), Arc::clone(&answer));
                Box::pin(async move {
                    if request.mentions(HOLD) {
                        gate.acquire()
                            .await
                            .expect("the gate is never closed")
                            .forget();
                    }
                    answer(&request, index)
                })
            },
        );
        Self {
            stand_in: StandIn::start(responder).await,
            gate,
        }
    }

    fn open(&self, calls: usize) {
        self.gate.add_permits(calls);
    }

    fn calls(&self) -> usize {
        self.stand_in.requests().len()
    }

    async fn wait_calls(&self, count: usize) {
        let what = format!("{count} model calls at the stand-in gateway");
        service::wait_for(&what, WAIT, || async {
            (self.calls() >= count).then_some(())
        })
        .await;
    }
}

struct World {
    product: Arc<ProductPostgres>,
    issuer: TestIssuer,
    gateway: Gateway,
    env: Vec<(String, String)>,
    service: Service,
}

/// The service with the stand-in gateway answering with `answer`, `env` on top; `None` without
/// a container runtime.
async fn world_with(
    answer: impl Fn(&Recorded, usize) -> support::Reply + Send + Sync + 'static,
    env: &[(&str, &str)],
) -> Option<World> {
    let product = product_postgres(&["assistant_app", "assistant_platform"]).await?;
    let issuer = TestIssuer::new();
    let gateway = Gateway::start(answer).await;
    let mut vars = vec![(
        "TRUSTIOT__AI_GATEWAY__URL".to_owned(),
        gateway.stand_in.url.clone(),
    )];
    vars.extend(
        env.iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned())),
    );
    let service = start(&product, &issuer, &vars).await;
    Some(World {
        product,
        issuer,
        gateway,
        env: vars,
        service,
    })
}

/// The service whose model answers every call with the recorded answer, with the answer tool
/// switched off (the default is on) so that its conversations answer as text.
async fn world() -> Option<World> {
    world_with(
        |_, _| ok(fixture("answer_end_turn.json")),
        &[(ANSWER_TOOL_SWITCH, "false")],
    )
    .await
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

fn stream_path(id: &str, run: &str) -> String {
    STREAM_PATH
        .replace("{conversation_id}", id)
        .replace("{run_id}", run)
}

impl World {
    /// Another replica on the same database and stand-ins, with `env` on top of this one's.
    async fn replica_with(&self, env: &[(&str, &str)]) -> Service {
        let mut vars = self.env.clone();
        vars.extend(
            env.iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned())),
        );
        start(&self.product, &self.issuer, &vars).await
    }

    async fn conversation(&self, user: &User) -> String {
        self.conversation_on(&self.service, user).await
    }

    /// Starts a conversation on `service`; its ID.
    async fn conversation_on(&self, service: &Service, user: &User) -> String {
        let reply = service
            .as_user(
                &self.issuer,
                user,
                Method::POST,
                CONVERSATIONS_PATH,
                Some(&json!({})),
            )
            .await;
        reply.expect_status(201);
        reply.json()["id"].as_str().expect("an ID").to_owned()
    }

    /// Asks `question` on `service`; the run's ID.
    async fn asked_on(&self, service: &Service, user: &User, id: &str, question: &str) -> String {
        let reply = service
            .as_user(
                &self.issuer,
                user,
                Method::POST,
                &QUESTIONS_PATH.replace("{conversation_id}", id),
                Some(&json!({ "question": question })),
            )
            .await;
        reply.expect_status(202);
        reply.json()["id"].as_str().expect("a run ID").to_owned()
    }

    async fn asked(&self, user: &User, id: &str, question: &str) -> String {
        self.asked_on(&self.service, user, id, question).await
    }

    async fn read(&self, user: &User, id: &str) -> Value {
        let reply = self
            .service
            .as_user(
                &self.issuer,
                user,
                Method::GET,
                &CONVERSATION_PATH.replace("{conversation_id}", id),
                None,
            )
            .await;
        reply.expect_status(200);
        reply.json()
    }

    /// Reads the conversation until its latest run is `run` with `status`.
    async fn wait_run(&self, user: &User, id: &str, run: &str, status: &str) -> Value {
        let what = format!("question {run} reading as {status}");
        service::wait_for(&what, WAIT, || async {
            let detail = self.read(user, id).await;
            let latest = &detail["latest_run"];
            (latest["id"] == run && latest["status"] == status).then_some(detail)
        })
        .await
    }

    /// Opens the answer stream of `run` on `base_url` as `user`, with `last_event_id`.
    async fn open(
        &self,
        base_url: &str,
        user: &User,
        id: &str,
        run: &str,
        last_event_id: Option<&str>,
    ) -> Events {
        open_with(base_url, &user.token(&self.issuer), id, run, last_event_id).await
    }

    /// The same as a plain request, for a refusal (the body is read whole).
    async fn refused(
        &self,
        service: &Service,
        user: &User,
        id: &str,
        run: &str,
        last_event_id: Option<&str>,
    ) -> Reply {
        let mut request = reqwest::Client::new()
            .get(format!("{}{}", service.base_url, stream_path(id, run)))
            .header("x-internal-identity", user.token(&self.issuer))
            .timeout(WAIT);
        if let Some(last) = last_event_id {
            request = request.header("last-event-id", last);
        }
        let response = request.send().await.expect("call the assistant");
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        let body = response.text().await.expect("read the body");
        Reply {
            status,
            headers,
            body,
        }
    }

    fn logs(&self, service: &Service) -> String {
        format!("{}\n{}", service.stdout.text(), service.stderr.text())
    }
}

async fn open_with(
    base_url: &str,
    token: &str,
    id: &str,
    run: &str,
    last_event_id: Option<&str>,
) -> Events {
    let mut request = reqwest::Client::new()
        .get(format!("{base_url}{}", stream_path(id, run)))
        .header("x-internal-identity", token);
    if let Some(last) = last_event_id {
        request = request.header("last-event-id", last);
    }
    let response = tokio::time::timeout(WAIT, request.send())
        .await
        .expect("the stream's head within the bound")
        .expect("open the stream");
    assert_eq!(response.status().as_u16(), 200, "the stream opens");
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("text/event-stream; charset=utf-8")
    );
    assert_eq!(
        response
            .headers()
            .get("cache-control")
            .and_then(|value| value.to_str().ok()),
        Some("no-store")
    );
    Events {
        response,
        buffer: Vec::new(),
        raw: Vec::new(),
    }
}

/// One server-sent event as the client parses it.
#[derive(Clone, Debug, Default)]
struct Event {
    event: String,
    id: Option<String>,
    data: String,
    retry: Option<String>,
}

impl Event {
    fn json(&self) -> Value {
        serde_json::from_str(&self.data).expect("an event's data is JSON")
    }
}

/// An open stream, read event by event.
struct Events {
    response: reqwest::Response,
    buffer: Vec<u8>,
    /// Every byte read, to look for what must never be in it.
    raw: Vec<u8>,
}

impl Events {
    /// The next event; `None` once the server closed the stream.
    async fn next(&mut self) -> Option<Event> {
        loop {
            if let Some(end) = self.buffer.windows(2).position(|pair| pair == b"\n\n") {
                let frame: Vec<u8> = self.buffer.drain(..end + 2).collect();
                return Some(parse(&frame));
            }
            let chunk = tokio::time::timeout(WAIT, self.response.chunk())
                .await
                .expect("the next piece of the stream within the bound");
            match chunk {
                Ok(Some(chunk)) => {
                    self.buffer.extend_from_slice(&chunk);
                    self.raw.extend_from_slice(&chunk);
                }
                Ok(None) | Err(_) => {
                    assert!(self.buffer.is_empty(), "a frame cut off: {:?}", self.buffer);
                    return None;
                }
            }
        }
    }

    /// Events until the stream closes.
    async fn rest(&mut self) -> Vec<Event> {
        let mut events = Vec::new();
        while let Some(event) = self.next().await {
            events.push(event);
        }
        events
    }

    /// The next event that is not a heartbeat.
    async fn next_content(&mut self) -> Option<Event> {
        loop {
            let event = self.next().await?;
            if event.event != "heartbeat" {
                return Some(event);
            }
        }
    }
}

fn parse(frame: &[u8]) -> Event {
    let text = std::str::from_utf8(frame).expect("an event is UTF-8");
    let mut event = Event::default();
    for line in text.lines() {
        let (field, value) = line.split_once(": ").unwrap_or((line, ""));
        match field {
            "event" => event.event = value.to_owned(),
            "id" => event.id = Some(value.to_owned()),
            "data" => event.data = value.to_owned(),
            "retry" => event.retry = Some(value.to_owned()),
            "" => {}
            other => panic!("an unknown field {other:?} in {text:?}"),
        }
    }
    event
}

/// The text of the `text` events of `events`, checking that each starts where the last ended
/// (from `offset`) and its `id` is where it ends; the text and the last `id`.
fn joined(events: &[Event], offset: u64) -> (String, u64) {
    let mut text = String::new();
    let mut at = offset;
    for event in events.iter().filter(|event| event.event == "text") {
        let data = event.json();
        assert_eq!(data["offset"].as_u64(), Some(at), "no gap and no overlap");
        let piece = data["text"].as_str().expect("text");
        let units = u64::try_from(piece.encode_utf16().count()).expect("a u64");
        assert!(units <= u64::try_from(TEXT_CHUNK_UNITS).expect("a u64"));
        at += units;
        assert_eq!(event.id.as_deref(), Some(at.to_string().as_str()));
        text.push_str(piece);
    }
    (text, at)
}

/// The answer the conversation shows last.
fn shown_answer(detail: &Value) -> String {
    let messages = detail["messages"].as_array().expect("messages");
    let last = messages.last().expect("an answer");
    assert_eq!(last["kind"], "answer");
    last["text"].as_str().expect("its text").to_owned()
}

/// A stand-in for an idle-cutting proxy (Cloudflare's 125 seconds): forwards each connection to
/// `upstream` and cuts it, both ways at once, when nothing passed either way for `idle`. Counts
/// the connections it cut.
struct IdleProxy {
    url: String,
    cut: Arc<AtomicUsize>,
}

impl IdleProxy {
    async fn start(upstream: &str, idle: Duration) -> Self {
        let upstream = upstream
            .strip_prefix("http://")
            .expect("a plain http origin")
            .to_owned();
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a loopback port");
        let address = listener.local_addr().expect("its address");
        let cut = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&cut);
        tokio::spawn(async move {
            while let Ok((client, _)) = listener.accept().await {
                let upstream = upstream.clone();
                let counted = Arc::clone(&counted);
                tokio::spawn(async move {
                    let Ok(server) = TcpStream::connect(&upstream).await else {
                        return;
                    };
                    if relay(client, server, idle).await {
                        counted.fetch_add(1, Ordering::SeqCst);
                    }
                });
            }
        });
        Self {
            url: format!("http://{address}"),
            cut,
        }
    }

    fn cut(&self) -> usize {
        self.cut.load(Ordering::SeqCst)
    }
}

/// Relays between `client` and `server` until either closes (false) or nothing passed for `idle`
/// (true: the proxy cut it).
async fn relay(client: TcpStream, server: TcpStream, idle: Duration) -> bool {
    let (mut client_read, mut client_write) = client.into_split();
    let (mut server_read, mut server_write) = server.into_split();
    let (mut up, mut down) = (vec![0_u8; 16 << 10], vec![0_u8; 16 << 10]);
    loop {
        tokio::select! {
            read = client_read.read(&mut up) => match read {
                Ok(0) | Err(_) => return false,
                Ok(n) => if server_write.write_all(&up[..n]).await.is_err() { return false },
            },
            read = server_read.read(&mut down) => match read {
                Ok(0) | Err(_) => return false,
                Ok(n) => if client_write.write_all(&down[..n]).await.is_err() { return false },
            },
            () = tokio::time::sleep(idle) => return true,
        }
    }
}

/// Acceptance criteria 1 and 5: a stream cut in the middle of the answer goes on, after a
/// reconnect with the last `id`, from where it was cut: no text twice and none missing, on the
/// same replica and on another; the text is the stored answer, character for character; a
/// reconnect after the whole answer gets only the end.
#[tokio::test(flavor = "multi_thread")]
async fn a_cut_stream_resumes_where_it_was_cut_on_any_replica() {
    let answer = long_answer();
    let reply = answer_with(&answer);
    let Some(world) = world_with(move |_, _| ok(reply.clone()), &[]).await else {
        return;
    };
    let other = world.replica_with(&[]).await;
    let owner = User::new(TenantId::generate(), TenantRole::Viewer);
    let id = world.conversation(&owner).await;
    let run = world.asked(&owner, &id, "How did floor 3 do?").await;
    let detail = world.wait_run(&owner, &id, &run, "answered").await;
    let stored = shown_answer(&detail);
    assert_eq!(stored, answer, "nothing removed from plain prose");

    // The first piece, then the connection drops.
    let mut first = world
        .open(&world.service.base_url, &owner, &id, &run, None)
        .await;
    let heartbeat = first.next().await.expect("the first frame");
    assert_eq!(heartbeat.event, "heartbeat");
    assert_eq!(heartbeat.retry.as_deref(), Some("1000"));
    let piece = first.next_content().await.expect("the first piece");
    let (head, cut_at) = joined(std::slice::from_ref(&piece), 0);
    let answer_units = u64::try_from(answer.encode_utf16().count()).expect("a length");
    assert!(cut_at > 0 && cut_at < answer_units);
    drop(first);

    // The rest from the other replica, from where it was cut.
    let mut resumed = world
        .open(&other.base_url, &owner, &id, &run, piece.id.as_deref())
        .await;
    let events = resumed.rest().await;
    let (tail, end_at) = joined(&events, cut_at);
    assert_eq!(format!("{head}{tail}"), stored, "the whole answer, once");
    assert_eq!(
        usize::try_from(end_at).expect("a length"),
        stored.encode_utf16().count()
    );
    let end = events.last().expect("the end");
    assert_eq!(end.event, "end");
    assert_eq!(end.json()["status"], "answered");
    assert_eq!(end.json()["id"], run.as_str());
    assert!(
        events.iter().filter(|event| event.event == "text").count() >= 2,
        "the rest came in more than one piece"
    );

    // From the middle of a later piece, on this replica.
    let middle = cut_at + 7;
    let mut again = world
        .open(
            &world.service.base_url,
            &owner,
            &id,
            &run,
            Some(&middle.to_string()),
        )
        .await;
    let (rest, _) = joined(&again.rest().await, middle);
    let skip = stored
        .char_indices()
        .scan(0_u64, |units, (index, character)| {
            let at = *units;
            *units += u64::try_from(character.len_utf16()).expect("a u64");
            Some((at, index))
        })
        .find(|(at, _)| *at == middle)
        .map(|(_, index)| index)
        .expect("a character starts there");
    assert_eq!(Some(rest.as_str()), stored.get(skip..));

    // After the whole answer: only the end.
    let mut done = world
        .open(
            &other.base_url,
            &owner,
            &id,
            &run,
            Some(&end_at.to_string()),
        )
        .await;
    let events = done.rest().await;
    let kinds: Vec<&str> = events.iter().map(|event| event.event.as_str()).collect();
    assert_eq!(kinds, ["heartbeat", "end"]);
    println!(
        "cut after {cut_at} of {end_at} code units; resumed on another replica with nothing twice \
         or missing"
    );
}

/// Acceptance criteria 2 and 5: opening the stream while the question runs (a reopened page, a
/// second tab, another replica) shows it running: the first heartbeat comes at once, before the
/// question answers, the conversation's `latest_run` is `running`, and each stream gets the
/// whole answer and the end once it answers. The stream on the replica that does not run the
/// question learns of the end through its poll.
#[tokio::test(flavor = "multi_thread")]
async fn a_reopened_page_sees_the_running_question_and_gets_its_answer() {
    let Some(world) = world().await else {
        return;
    };
    let other = world.replica_with(&[]).await;
    let owner = User::new(TenantId::generate(), TenantRole::Viewer);
    let id = world.conversation(&owner).await;
    let run = world
        .asked(&owner, &id, &format!("Did floor 3 report? {HOLD}"))
        .await;
    world.gateway.wait_calls(1).await;

    let mut here = world
        .open(&world.service.base_url, &owner, &id, &run, None)
        .await;
    let mut there = world.open(&other.base_url, &owner, &id, &run, None).await;
    for stream in [&mut here, &mut there] {
        let first = stream.next().await.expect("the first frame");
        assert_eq!(first.event, "heartbeat", "a heartbeat at once");
        assert_eq!(first.json(), json!({}));
    }
    let running = world.read(&owner, &id).await;
    assert_eq!(running["latest_run"]["id"], run.as_str());
    assert_eq!(running["latest_run"]["status"], "running");

    world.gateway.open(1);
    let answered = world.wait_run(&owner, &id, &run, "answered").await;
    let stored = shown_answer(&answered);
    for (name, stream) in [("this replica", &mut here), ("the other", &mut there)] {
        let events = stream.rest().await;
        let (text, _) = joined(&events, 0);
        assert_eq!(text, stored, "{name}");
        let end = events.last().expect("the end");
        assert_eq!(end.event, "end", "{name}");
        assert_eq!(end.json()["status"], "answered", "{name}");
    }
    println!("two streams opened while the question ran each got the whole answer and its end");
}

/// Acceptance criterion 3, with the stop of Laysi/trustiot-v2#646: stopping the question ends
/// its stream with `cancelled` and no text, and no model call follows.
#[tokio::test(flavor = "multi_thread")]
async fn a_stop_ends_the_stream_with_cancelled() {
    let Some(world) = world().await else {
        return;
    };
    let owner = User::new(TenantId::generate(), TenantRole::Viewer);
    let id = world.conversation(&owner).await;
    let run = world
        .asked(&owner, &id, &format!("Compare every meter {HOLD}"))
        .await;
    world.gateway.wait_calls(1).await;
    let mut stream = world
        .open(&world.service.base_url, &owner, &id, &run, None)
        .await;
    assert_eq!(stream.next().await.expect("a heartbeat").event, "heartbeat");

    world
        .service
        .as_user(
            &world.issuer,
            &owner,
            Method::POST,
            &STOP_PATH
                .replace("{conversation_id}", &id)
                .replace("{run_id}", &run),
            Some(&json!({})),
        )
        .await
        .expect_status(202);
    let events = stream.rest().await;
    assert!(events.iter().all(|event| event.event != "text"), "no text");
    let end = events.last().expect("the end");
    assert_eq!(end.event, "end");
    assert_eq!(end.json()["status"], "cancelled");
    assert_eq!(end.json()["message_key"], "question.cancelled");
    assert_eq!(world.gateway.calls(), 1, "no model call after the stop");
    world.gateway.open(1);
    println!("the stop ended the stream with cancelled and the question with no further call");
}

/// Acceptance criterion 4: the stream carries the stored answer and nothing else: not the text,
/// thinking or tool input of a model turn that went on to call a tool, not the answer's
/// thinking, not the SQL fence the answer guard removed, not the prompt or the question.
#[tokio::test(flavor = "multi_thread")]
async fn the_stream_carries_neither_steps_nor_prompt_nor_sql() {
    let answer = format!(
        "Floor 3 used 412 kWh yesterday.\n\n```sql\nSELECT kwh FROM {SQL_MARKER}\n```\n\nThe \
         query SELECT device_id, kwh FROM {SQL_MARKER} WHERE floor = 3 gave it.\n\nThat is \
         within the usual range."
    );
    let step = narrated_step();
    let reply = answer_with(&answer);
    let Some(world) = world_with(
        move |_, index| {
            ok(if index == 0 {
                step.clone()
            } else {
                reply.clone()
            })
        },
        &[],
    )
    .await
    else {
        return;
    };
    let owner = User::new(TenantId::generate(), TenantRole::Viewer);
    let id = world.conversation(&owner).await;
    let question = format!("How much did floor 3 use? {HOLD}");
    let run = world.asked(&owner, &id, &question).await;
    world.gateway.wait_calls(1).await;
    let mut stream = world
        .open(&world.service.base_url, &owner, &id, &run, None)
        .await;
    world.gateway.open(2);
    let events = stream.rest().await;
    assert_eq!(world.gateway.calls(), 2, "a step, then the answer");

    let detail = world.wait_run(&owner, &id, &run, "answered").await;
    let stored = shown_answer(&detail);
    assert_eq!(detail["messages"][1]["removed"], json!(["code", "sql"]));
    let (text, _) = joined(&events, 0);
    assert_eq!(text, stored, "the stream is the stored answer");
    assert!(text.contains("412 kWh") && text.contains("usual range"));
    let raw = String::from_utf8(stream.raw.clone()).expect("UTF-8");
    for marker in [
        STEP_MARKER,
        STEP_THINKING_MARKER,
        TOOL_INPUT_MARKER,
        ANSWER_THINKING_MARKER,
        SQL_MARKER,
        "SELECT",
        "```",
        "count_readings",
        "You are the TrustIoT assistant",
        HOLD,
    ] {
        assert!(!raw.contains(marker), "{marker:?} is in the stream:\n{raw}");
    }
    println!(
        "the stream held the stored answer only: no step, thinking, tool input, prompt or SQL"
    );
}

/// Acceptance criterion 6 and the 125-second cut: through a proxy that cuts a connection idle for
/// [`PROXY_IDLE`], a question that runs many times that long keeps its stream, since a heartbeat
/// goes out at once and then every [`FAST_HEARTBEAT`]; the stream gets the answer and the end. A
/// stream whose heartbeats come further apart than the proxy's idle limit (the default 20
/// seconds here) is cut by the same proxy, with no end: the proxy does cut.
#[tokio::test(flavor = "multi_thread")]
async fn heartbeats_keep_a_long_question_open_through_an_idle_cutting_proxy() {
    let Some(world) = world_with(
        |_, _| ok(fixture("answer_end_turn.json")),
        &[("TRUSTIOT__STREAMS__HEARTBEAT_INTERVAL", FAST_HEARTBEAT)],
    )
    .await
    else {
        return;
    };
    let proxy = IdleProxy::start(&world.service.base_url, PROXY_IDLE).await;
    let owner = User::new(TenantId::generate(), TenantRole::Viewer);
    let id = world.conversation(&owner).await;
    let run = world
        .asked(&owner, &id, &format!("A long analysis {HOLD}"))
        .await;
    world.gateway.wait_calls(1).await;

    let mut stream = world.open(&proxy.url, &owner, &id, &run, None).await;
    let started = Instant::now();
    for count in 0..HEARTBEATS_PAST_THE_IDLE_CUT {
        let event = stream.next().await.expect("the stream stays open");
        assert_eq!(event.event, "heartbeat", "heartbeat {count}");
    }
    assert!(
        started.elapsed() > PROXY_IDLE * 2,
        "the question ran past the proxy's idle cut more than once"
    );
    world.gateway.open(1);
    let events = stream.rest().await;
    let end = events.last().expect("the end");
    assert_eq!(end.event, "end");
    assert_eq!(end.json()["status"], "answered");
    assert_eq!(proxy.cut(), 0, "the proxy never found the stream idle");

    // The control: a replica with the default heartbeat (20 s) behind the same kind of proxy.
    let slow = world
        .replica_with(&[("TRUSTIOT__STREAMS__HEARTBEAT_INTERVAL", "20s")])
        .await;
    let slow_proxy = IdleProxy::start(&slow.base_url, PROXY_IDLE).await;
    let conversation = world.conversation(&owner).await;
    let held = world
        .asked(&owner, &conversation, &format!("Another {HOLD}"))
        .await;
    let mut cut = world
        .open(&slow_proxy.url, &owner, &conversation, &held, None)
        .await;
    assert_eq!(
        cut.next().await.expect("the first heartbeat").event,
        "heartbeat"
    );
    assert!(cut.next().await.is_none(), "the proxy cut it, with no end");
    assert_eq!(slow_proxy.cut(), 1);
    world.gateway.open(1);
    println!(
        "{HEARTBEATS_PAST_THE_IDLE_CUT} heartbeats kept the stream through a {PROXY_IDLE:?} idle \
         cut; without them the proxy cut it"
    );
}

/// A stream lives no longer than the identity token it was opened with: it closes without an
/// end while the question goes on, and a reconnect with a new token gets the answer.
#[tokio::test(flavor = "multi_thread")]
async fn a_stream_closes_when_its_token_expires_and_a_reconnect_goes_on() {
    let Some(world) = world().await else {
        return;
    };
    let owner = User::new(TenantId::generate(), TenantRole::Viewer);
    let id = world.conversation(&owner).await;
    let run = world
        .asked(&owner, &id, &format!("A long question {HOLD}"))
        .await;
    world.gateway.wait_calls(1).await;

    let mut claims = owner.claims(ServiceName::Assistant);
    let now = i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("after the epoch")
            .as_secs(),
    )
    .expect("seconds fit");
    claims.iat = now;
    claims.exp = now + 2;
    let short = world.issuer.token(&claims);
    let mut stream = open_with(&world.service.base_url, &short, &id, &run, None).await;
    let events = stream.rest().await;
    assert!(
        events.iter().all(|event| event.event == "heartbeat"),
        "closed with no end: {events:?}"
    );
    let detail = world.read(&owner, &id).await;
    assert_eq!(
        detail["latest_run"]["status"], "running",
        "the question goes on"
    );

    let mut again = world
        .open(&world.service.base_url, &owner, &id, &run, None)
        .await;
    world.gateway.open(1);
    let events = again.rest().await;
    assert_eq!(events.last().expect("the end").json()["status"], "answered");
    println!("the stream closed with its token; the reconnect got the answer");
}

/// Who may open a stream: only the conversation's owner, for a run of that conversation, while
/// it is not deleted; everything else is the one 404, as for a conversation that does not exist.
/// Deleting the conversation during the stream closes it with no end, and the reconnect is 404.
/// A `Last-Event-ID` that is no position of the answer is 400.
#[tokio::test(flavor = "multi_thread")]
async fn only_the_owner_streams_and_a_deleted_conversation_is_not_found() {
    let Some(world) = world().await else {
        return;
    };
    let tenant = TenantId::generate();
    let owner = User::new(tenant, TenantRole::Viewer);
    let id = world.conversation(&owner).await;
    let other_conversation = world.conversation(&owner).await;
    let run = world
        .asked(&owner, &id, &format!("The owner's question {HOLD}"))
        .await;
    world.gateway.wait_calls(1).await;
    world
        .service
        .as_user(
            &world.issuer,
            &owner,
            Method::PUT,
            &SHARE_PATH.replace("{conversation_id}", &id),
            None,
        )
        .await
        .expect_status(200);

    let missing = world
        .refused(
            &world.service,
            &owner,
            &Uuid::now_v7().to_string(),
            &run,
            None,
        )
        .await;
    assert_eq!(missing.status, 404, "{}", missing.body);
    let not_found = (missing.status, missing.body.clone());
    let reader = User::new(tenant, TenantRole::Admin);
    let stranger = User::new(TenantId::generate(), TenantRole::Admin);
    let owner_elsewhere = User {
        tenant: TenantId::generate(),
        ..owner
    };
    for (who, user, conversation, run) in [
        (
            "the owner in another tenant",
            &owner_elsewhere,
            id.as_str(),
            run.as_str(),
        ),
        (
            "a reader of the shared conversation",
            &reader,
            id.as_str(),
            run.as_str(),
        ),
        (
            "another tenant's user",
            &stranger,
            id.as_str(),
            run.as_str(),
        ),
        (
            "a run of another conversation",
            &owner,
            other_conversation.as_str(),
            run.as_str(),
        ),
        (
            "an unknown run",
            &owner,
            id.as_str(),
            &Uuid::now_v7().to_string(),
        ),
        (
            "a run ID that is not a UUID",
            &owner,
            id.as_str(),
            "not-a-uuid",
        ),
    ] {
        let reply = world
            .refused(&world.service, user, conversation, run, None)
            .await;
        assert_eq!((reply.status, reply.body.clone()), not_found, "{who}");
    }
    for last in ["abc", "-1", "1", "99999999999"] {
        world
            .refused(&world.service, &owner, &id, &run, Some(last))
            .await
            .expect_error(400, "invalid_request", "stream.resume_invalid");
    }

    let mut stream = world
        .open(&world.service.base_url, &owner, &id, &run, None)
        .await;
    assert_eq!(stream.next().await.expect("a heartbeat").event, "heartbeat");
    world
        .service
        .as_user(
            &world.issuer,
            &owner,
            Method::DELETE,
            &CONVERSATION_PATH.replace("{conversation_id}", &id),
            None,
        )
        .await
        .expect_status(204);
    let events = stream.rest().await;
    assert!(
        events.iter().all(|event| event.event == "heartbeat"),
        "closed with no end: {events:?}"
    );
    let reply = world.refused(&world.service, &owner, &id, &run, None).await;
    assert_eq!(
        (reply.status, reply.body.clone()),
        not_found,
        "after the delete"
    );
    println!("only the owner streams; a deleted conversation closes its stream and is 404 after");
}

/// The limits: a user holds at most `streams.max_per_user` streams on a replica (429), the users
/// of one tenant together at most `streams.max_per_tenant` (429, so the other tenants keep the
/// rest of the replica), a replica at most `streams.max_per_replica` (503 with `Retry-After`); a
/// closed stream gives its slot back, and a refused request is logged without the
/// conversation's content.
#[tokio::test(flavor = "multi_thread")]
async fn a_replica_a_tenant_and_a_user_hold_at_most_their_streams() {
    let Some(world) = world_with(
        |_, _| ok(fixture("answer_end_turn.json")),
        &[
            ("TRUSTIOT__STREAMS__MAX_PER_USER", "1"),
            ("TRUSTIOT__STREAMS__MAX_PER_TENANT", "2"),
            ("TRUSTIOT__STREAMS__MAX_PER_REPLICA", "3"),
            ("TRUSTIOT__STREAMS__HEARTBEAT_INTERVAL", FAST_HEARTBEAT),
        ],
    )
    .await
    else {
        return;
    };
    let busy_tenant = TenantId::generate();
    let (alice, bob, carol) = (
        User::new(busy_tenant, TenantRole::Viewer),
        User::new(busy_tenant, TenantRole::Viewer),
        User::new(busy_tenant, TenantRole::Viewer),
    );
    let dave = User::new(TenantId::generate(), TenantRole::Viewer);
    let erin = User::new(TenantId::generate(), TenantRole::Viewer);
    let mut runs = Vec::new();
    for user in [&alice, &bob, &carol, &dave, &erin] {
        let id = world.conversation(user).await;
        let run = world
            .asked(user, &id, &format!("A held question {HOLD}"))
            .await;
        runs.push((id, run));
    }
    let base = world.service.base_url.clone();
    let alices = world
        .open(&base, &alice, &runs[0].0, &runs[0].1, None)
        .await;
    let user_limited = world
        .refused(&world.service, &alice, &runs[0].0, &runs[0].1, None)
        .await;
    user_limited.expect_error(429, "rate_limited", "stream.limited");
    assert_eq!(user_limited.json()["details"]["max"], 1);
    let mut bobs = world.open(&base, &bob, &runs[1].0, &runs[1].1, None).await;
    // The tenant's two slots are taken: its third user is refused, within their own limit.
    let tenant_limited = world
        .refused(&world.service, &carol, &runs[2].0, &runs[2].1, None)
        .await;
    tenant_limited.expect_error(429, "rate_limited", "stream.limited");
    assert_eq!(tenant_limited.json()["details"]["max"], 2);
    let logged = world
        .service
        .wait_logged("answer stream refused: the tenant holds as many streams as one tenant may")
        .await;
    assert_eq!(logged["fields"]["max"], 2);
    // Another tenant still gets the replica's last slot; then the replica is full.
    let daves = world.open(&base, &dave, &runs[3].0, &runs[3].1, None).await;
    let busy = world
        .refused(&world.service, &erin, &runs[4].0, &runs[4].1, None)
        .await;
    busy.expect_error(503, "unavailable", "stream.busy");
    assert_eq!(busy.header("retry-after"), Some("5"));
    let logged = world
        .service
        .wait_logged("answer stream refused: the replica holds as many streams as it may")
        .await;
    assert_eq!(logged["fields"]["max"], 3);

    drop(alices);
    let what = "the closed stream's slot given back to its tenant";
    service::wait_for(what, WAIT, || async {
        let reply = world.refused_head(&carol, &runs[2].0, &runs[2].1).await;
        (reply == 200).then_some(())
    })
    .await;
    assert_eq!(
        bobs.next().await.expect("bob's stream is open").event,
        "heartbeat"
    );
    drop(daves);
    world.gateway.open(5);
    let logs = world.logs(&world.service);
    assert!(!logs.contains(HOLD), "no question in the logs");
    println!(
        "one stream per user, two per tenant, three per replica; a closed stream's slot came back"
    );
}

impl World {
    /// The status of opening `run`'s stream as `user`, closing it at once if it opened.
    async fn refused_head(&self, user: &User, id: &str, run: &str) -> u16 {
        let response = reqwest::Client::new()
            .get(format!("{}{}", self.service.base_url, stream_path(id, run)))
            .header("x-internal-identity", user.token(&self.issuer))
            .timeout(WAIT)
            .send()
            .await
            .expect("call the assistant");
        response.status().as_u16()
    }
}

/// A restart: the replica that runs the question shuts down; its stream closes with no end, the
/// question is recorded `interrupted` (Laysi/trustiot-v2#646), and the reconnect to another
/// replica gets that end.
#[tokio::test(flavor = "multi_thread")]
async fn a_restart_closes_the_stream_and_the_reconnect_reads_interrupted() {
    let Some(mut world) = world().await else {
        return;
    };
    let other = world.replica_with(&[]).await;
    let owner = User::new(TenantId::generate(), TenantRole::Viewer);
    let id = world.conversation(&owner).await;
    let run = world
        .asked(&owner, &id, &format!("Summarise the year {HOLD}"))
        .await;
    world.gateway.wait_calls(1).await;
    let mut stream = world
        .open(&world.service.base_url, &owner, &id, &run, None)
        .await;
    assert_eq!(stream.next().await.expect("a heartbeat").event, "heartbeat");

    world.service.terminate();
    let events = stream.rest().await;
    assert!(
        events.iter().all(|event| event.event == "heartbeat"),
        "closed with no end: {events:?}"
    );
    let status = world.service.wait_exit(WAIT).await;
    assert!(status.success(), "{status}");

    let mut again = world.open(&other.base_url, &owner, &id, &run, None).await;
    let events = again.rest().await;
    let end = events.last().expect("the end");
    assert_eq!(end.event, "end");
    assert_eq!(end.json()["status"], "interrupted");
    assert_eq!(end.json()["message_key"], "question.interrupted");
    world.gateway.open(1);
    println!("the restart closed the stream; the reconnect elsewhere read interrupted");
}

// Answers written with the answer tool (the user's decision of 2026-11-05): the paragraphs the
// model finished go out while the question runs, from the partial answer in Postgres.

/// Paragraphs of an answer written with the answer tool; the second has inline SQL, which the
/// answer guard removes.
const TOOL_FIRST: &str = "Floor 3 used 412 kWh last week, 8 % more than the week before.";
fn tool_second() -> String {
    format!("The rise comes from the chillers; the query `SELECT kwh FROM {SQL_MARKER}` shows it.")
}
const TOOL_THIRD: &str = "Nothing else changed.";

/// The model turn that writes `text` with the answer tool, in pieces, after thinking and text
/// that must never go out; each item is one event.
fn answer_tool_events(text: &str) -> (Vec<String>, Vec<String>) {
    let pieces = support::json_pieces(text, 7);
    let thinking = (
        json!({"type": "thinking", "thinking": "", "signature": ""}),
        vec![
            json!({"type": "thinking_delta", "thinking": ANSWER_THINKING_MARKER}),
            json!({"type": "signature_delta", "signature": "EqMBCkYICxIMtool"}),
        ],
    );
    let events = support::turn_events(
        &[
            thinking,
            support::text_block(&format!("Writing it up ({STEP_MARKER}).")),
            support::call_block(
                "toolu_answer_stream",
                ANSWER_TOOL,
                &pieces.iter().map(String::as_str).collect::<Vec<_>>(),
            ),
        ],
        "tool_use",
    );
    (events, pieces)
}

/// The events up to and including the piece of the answer's JSON that completes `needle`: the
/// message's start, the thinking (start, two deltas, stop), the text (start, delta, stop), the
/// call's start, then the pieces.
fn events_through(events: &[String], pieces: &[String], needle: &str) -> usize {
    let mut written = String::new();
    let piece = pieces
        .iter()
        .position(|piece| {
            written.push_str(piece);
            written.contains(needle)
        })
        .expect("the needle is in the answer");
    let through = 1 + 4 + 3 + 1 + piece + 1;
    assert!(through < events.len());
    through
}

/// The switch that gives new conversations the answer tool; on by default.
const ANSWER_TOOL_SWITCH: &str = "TRUSTIOT__STREAMS__ANSWER_TOOL";

/// The model's replies in order; the recorded answer once they are used up, on a service whose
/// new conversations get the answer tool.
type Script = Arc<std::sync::Mutex<std::collections::VecDeque<support::Reply>>>;

async fn scripted_world() -> Option<(World, Script)> {
    let script: Script = Arc::default();
    let replies = Arc::clone(&script);
    let world = world_with(
        move |_, _| {
            replies
                .lock()
                .expect("the lock")
                .pop_front()
                .unwrap_or_else(|| ok(fixture("answer_end_turn.json")))
        },
        &[(ANSWER_TOOL_SWITCH, "true")],
    )
    .await?;
    Some((world, script))
}

/// Text events until their text is `expected` (from `offset`), skipping heartbeats; the last
/// `id`.
async fn text_until(stream: &mut Events, offset: u64, expected: &str) -> u64 {
    let mut events = Vec::new();
    loop {
        let event = stream
            .next_content()
            .await
            .expect("text before the stream closes");
        assert_eq!(event.event, "text", "{event:?}");
        events.push(event);
        let (text, at) = joined(&events, offset);
        if text == expected {
            return at;
        }
        assert!(
            expected.starts_with(&text),
            "{text:?} is not the start of {expected:?}"
        );
    }
}

fn units(text: &str) -> u64 {
    u64::try_from(text.encode_utf16().count()).expect("a length")
}

/// The decision's acceptance criteria with acceptance criteria 1, 2 and 5: the paragraphs the
/// model finished in the answer tool go out while the question still runs, on the replica that
/// runs it and on another; a page reopened mid-answer gets what went out so far, from the start;
/// a reconnect mid-answer on another replica goes on from its `id` with nothing twice; once the
/// question answers, each stream goes on into the stored answer seamlessly and ends. Nothing
/// outside the tool goes out: not the thinking, not the text before the call, not the SQL the
/// guard removes.
#[tokio::test(flavor = "multi_thread")]
async fn answer_tool_paragraphs_stream_while_the_question_runs_and_resume_anywhere() {
    let Some((world, script)) = scripted_world().await else {
        return;
    };
    let other = world.replica_with(&[]).await;
    let (sender, reply) = support::events_reply();
    script.lock().expect("the lock").push_back(reply);
    let text = format!("{TOOL_FIRST}\n\n{}\n\n{TOOL_THIRD}", tool_second());
    let (events, pieces) = answer_tool_events(&text);
    let first_cut = events_through(&events, &pieces, "\\n\\nThe rise");
    let second_cut = events_through(&events, &pieces, "\\n\\nNothing");

    let owner = User::new(TenantId::generate(), TenantRole::Viewer);
    let id = world.conversation(&owner).await;
    let run = world.asked(&owner, &id, "How much did floor 3 use?").await;
    for event in &events[..first_cut] {
        sender.send(event.clone()).await.expect("the gateway reads");
    }

    // The first paragraph goes out on the replica that runs the question, while it runs.
    let mut here = world
        .open(&world.service.base_url, &owner, &id, &run, None)
        .await;
    let first_id = text_until(&mut here, 0, TOOL_FIRST).await;
    assert_eq!(first_id, units(TOOL_FIRST));
    let detail = world.read(&owner, &id).await;
    assert_eq!(detail["latest_run"]["status"], "running");
    assert!(
        detail["messages"]
            .as_array()
            .expect("messages")
            .iter()
            .all(|message| message["kind"] != "answer"),
        "no answer is stored yet"
    );
    // A page reopened on the other replica gets it from the start; a reconnect there from the
    // first `id` gets nothing twice.
    let mut reopened = world.open(&other.base_url, &owner, &id, &run, None).await;
    text_until(&mut reopened, 0, TOOL_FIRST).await;
    let first_id_text = first_id.to_string();
    let mut resumed = world
        .open(&other.base_url, &owner, &id, &run, Some(&first_id_text))
        .await;

    // The second paragraph, cleaned by the guard, reaches all three.
    for event in &events[first_cut..second_cut] {
        sender.send(event.clone()).await.expect("the gateway reads");
    }
    let cleaned_second = {
        let guarded =
            assistant::answer_guard::guard_streamed(&format!("{TOOL_FIRST}\n\n{}", tool_second()));
        guarded
            .text
            .strip_prefix(TOOL_FIRST)
            .expect("the first paragraph stays")
            .to_owned()
    };
    assert!(!cleaned_second.contains(SQL_MARKER), "{cleaned_second:?}");
    for (name, stream) in [
        ("this replica", &mut here),
        ("the reopened page", &mut reopened),
        ("the reconnect", &mut resumed),
    ] {
        let at = text_until(stream, first_id, &cleaned_second).await;
        assert_eq!(at, first_id + units(&cleaned_second), "{name}");
    }

    // The rest: the question answers, and each stream goes on into the stored answer.
    for event in &events[second_cut..] {
        sender.send(event.clone()).await.expect("the gateway reads");
    }
    drop(sender);
    let answered = world.wait_run(&owner, &id, &run, "answered").await;
    let stored = shown_answer(&answered);
    let sent_before = format!("{TOOL_FIRST}{cleaned_second}");
    assert!(stored.starts_with(&sent_before), "{stored:?}");
    assert!(stored.ends_with(TOOL_THIRD));
    let at = first_id + units(&cleaned_second);
    for (name, stream) in [
        ("this replica", &mut here),
        ("the reopened page", &mut reopened),
        ("the reconnect", &mut resumed),
    ] {
        let events = stream.rest().await;
        let (tail, end_at) = joined(&events, at);
        assert_eq!(format!("{sent_before}{tail}"), stored, "{name}");
        assert_eq!(end_at, units(&stored), "{name}");
        let end = events.last().expect("the end");
        assert_eq!(end.event, "end", "{name}");
        assert_eq!(end.json()["status"], "answered", "{name}");
    }
    for (name, stream) in [
        ("this replica", &here),
        ("the reopened page", &reopened),
        ("the reconnect", &resumed),
    ] {
        let raw = String::from_utf8_lossy(&stream.raw);
        for marker in [
            STEP_MARKER,
            ANSWER_THINKING_MARKER,
            SQL_MARKER,
            "toolu_answer_stream",
        ] {
            assert!(!raw.contains(marker), "{name} carried {marker}");
        }
    }
    // The question's run left no partial answer behind.
    // Of this run only: the tests share the database.
    let mut admin = world.product.superuser().await;
    let partials: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM assistant.answer_partials WHERE run_id = $1::uuid",
    )
    .bind(&run)
    .fetch_one(&mut admin)
    .await
    .expect("count the partial answers");
    assert_eq!(partials, 0);
    println!(
        "two paragraphs went out before the question answered, on both replicas; the reconnect \
         went on from {first_id} with nothing twice"
    );
}

/// Acceptance criterion 3 mid-answer: a stop after paragraphs went out cancels the model's
/// stream on the server and ends every stream with `cancelled`; no answer is stored, and a
/// reconnect from the last `id` gets only the end, which tells the client to drop the text.
#[tokio::test(flavor = "multi_thread")]
async fn a_stop_in_the_middle_of_the_answer_cancels_it() {
    let Some((world, script)) = scripted_world().await else {
        return;
    };
    let (sender, reply) = support::events_reply();
    script.lock().expect("the lock").push_back(reply);
    let text = format!("{TOOL_FIRST}\n\n{}\n\n{TOOL_THIRD}", tool_second());
    let (events, pieces) = answer_tool_events(&text);
    let cut = events_through(&events, &pieces, "\\n\\nThe rise");
    let owner = User::new(TenantId::generate(), TenantRole::Viewer);
    let id = world.conversation(&owner).await;
    let run = world.asked(&owner, &id, "How much did floor 3 use?").await;
    for event in &events[..cut] {
        sender.send(event.clone()).await.expect("the gateway reads");
    }
    let mut stream = world
        .open(&world.service.base_url, &owner, &id, &run, None)
        .await;
    let first_id = text_until(&mut stream, 0, TOOL_FIRST).await;

    world
        .service
        .as_user(
            &world.issuer,
            &owner,
            Method::POST,
            &STOP_PATH
                .replace("{conversation_id}", &id)
                .replace("{run_id}", &run),
            Some(&json!({})),
        )
        .await
        .expect_status(202);
    let events_after = stream.rest().await;
    assert!(events_after.iter().all(|event| event.event != "text"));
    let end = events_after.last().expect("the end");
    assert_eq!(end.event, "end");
    assert_eq!(end.json()["status"], "cancelled");
    // The server dropped the model's stream: the gateway's sender sees its reader gone.
    tokio::time::timeout(WAIT, sender.closed())
        .await
        .expect("the model call was cancelled within the bound");
    let detail = world.wait_run(&owner, &id, &run, "cancelled").await;
    assert!(
        detail["messages"]
            .as_array()
            .expect("messages")
            .iter()
            .all(|message| message["kind"] != "answer"),
        "nothing of the round is stored"
    );
    let mut again = world
        .open(
            &world.service.base_url,
            &owner,
            &id,
            &run,
            Some(&first_id.to_string()),
        )
        .await;
    let kinds: Vec<String> = again
        .rest()
        .await
        .into_iter()
        .map(|event| event.event)
        .collect();
    assert_eq!(kinds, ["heartbeat", "end"]);
    println!("the stop mid-answer cancelled the model call and ended the stream with cancelled");
}

/// The review's scenario end to end (Laysi/trustiot-v2#147): the answer call gives `text` twice,
/// the second one empty (the API does not check an input it streams). Two paragraphs of the
/// first went out to the stream before the second key; the parser keeps the empty one. The
/// question fails `model_failed` without calling the model again, whether its next turn would
/// have answered with the tool or as text, neither starting with what went out: no answer is
/// stored, the partial answer is gone, and the stream ends `failed` with no text after what it
/// sent.
#[tokio::test(flavor = "multi_thread")]
async fn an_answer_whose_streamed_text_is_not_its_parsed_text_fails_without_an_answer() {
    let Some((world, script)) = scripted_world().await else {
        return;
    };
    let text_answer = {
        let mut message: Value =
            serde_json::from_str(&fixture("answer_end_turn.json")).expect("JSON");
        message["content"] = json!([{"type": "text", "text": "Q1.\n\nQ2."}]);
        message.to_string()
    };
    let released = format!("{TOOL_FIRST}\n\n{TOOL_THIRD}");
    let written = format!("{released}\n\nAnd one more paragraph.");
    for (name, next) in [
        (
            "a second tool answer",
            support::answer_step(1, "Q1.\n\nQ2."),
        ),
        ("a text answer", text_answer),
    ] {
        let (sender, reply) = support::events_reply();
        {
            let mut script = script.lock().expect("the lock");
            script.push_back(reply);
            script.push_back(ok(next));
        }
        // The answer's JSON with `text` given twice, in pieces.
        let once = json!({ "text": written }).to_string();
        let twice = format!(
            "{},\"text\":\"\"}}",
            once.strip_suffix('}').expect("an object")
        );
        let characters: Vec<char> = twice.chars().collect();
        let pieces: Vec<String> = characters
            .chunks(7)
            .map(|chunk| chunk.iter().collect())
            .collect();
        let events = support::turn_events(
            &[support::call_block(
                "toolu_twice",
                ANSWER_TOOL,
                &pieces.iter().map(String::as_str).collect::<Vec<_>>(),
            )],
            "tool_use",
        );
        // Through the third paragraph's first words: the message's start, the call's start and
        // its pieces up to there.
        let mut so_far = String::new();
        let piece = pieces
            .iter()
            .position(|piece| {
                so_far.push_str(piece);
                so_far.contains("\\n\\nAnd one")
            })
            .expect("the third paragraph starts");
        let cut = 1 + 1 + piece + 1;

        let owner = User::new(TenantId::generate(), TenantRole::Viewer);
        let id = world.conversation(&owner).await;
        let run = world.asked(&owner, &id, "How much did floor 3 use?").await;
        for event in &events[..cut] {
            sender.send(event.clone()).await.expect("the gateway reads");
        }
        let mut stream = world
            .open(&world.service.base_url, &owner, &id, &run, None)
            .await;
        text_until(&mut stream, 0, &released).await;
        for event in &events[cut..] {
            sender.send(event.clone()).await.expect("the gateway reads");
        }
        drop(sender);

        let detail = world.wait_run(&owner, &id, &run, "failed").await;
        assert_eq!(detail["latest_run"]["result"], "model_failed", "{name}");
        assert!(
            detail["messages"]
                .as_array()
                .expect("messages")
                .iter()
                .all(|message| message["kind"] != "answer"),
            "{name}: no answer is stored"
        );
        let rest = stream.rest().await;
        assert!(
            rest.iter().all(|event| event.event != "text"),
            "{name}: nothing more went out: {rest:?}"
        );
        let end = rest.last().expect("the end");
        assert_eq!(end.event, "end", "{name}");
        assert_eq!(end.json()["status"], "failed", "{name}");
        let raw = String::from_utf8_lossy(&stream.raw);
        assert!(!raw.contains("Q1."), "{name}");
        let mut admin = world.product.superuser().await;
        let partials: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM assistant.answer_partials WHERE run_id = $1::uuid",
        )
        .bind(&run)
        .fetch_one(&mut admin)
        .await
        .expect("count the partial answers");
        assert_eq!(
            partials, 0,
            "{name}: the run's end deleted its partial answer"
        );
        let unused = script.lock().expect("the lock").pop_front();
        assert!(
            unused.is_some(),
            "{name}: the model was not called for a second turn"
        );
    }
    println!(
        "an answer whose streamed text was not its parsed text failed the question, with no \
         answer, no partial answer and an end of failed"
    );
}

/// A restart mid-answer: the replica that runs the question shuts down after paragraphs went
/// out; the question is `interrupted` and a reconnect to another replica from the last `id`
/// gets only that end.
#[tokio::test(flavor = "multi_thread")]
async fn a_restart_in_the_middle_of_the_answer_interrupts_it() {
    let Some((mut world, script)) = scripted_world().await else {
        return;
    };
    let other = world.replica_with(&[]).await;
    let (sender, reply) = support::events_reply();
    script.lock().expect("the lock").push_back(reply);
    let text = format!("{TOOL_FIRST}\n\n{}\n\n{TOOL_THIRD}", tool_second());
    let (events, pieces) = answer_tool_events(&text);
    let cut = events_through(&events, &pieces, "\\n\\nThe rise");
    let owner = User::new(TenantId::generate(), TenantRole::Viewer);
    let id = world.conversation(&owner).await;
    let run = world.asked(&owner, &id, "How much did floor 3 use?").await;
    for event in &events[..cut] {
        sender.send(event.clone()).await.expect("the gateway reads");
    }
    let mut stream = world
        .open(&world.service.base_url, &owner, &id, &run, None)
        .await;
    let first_id = text_until(&mut stream, 0, TOOL_FIRST).await;

    world.service.terminate();
    let rest = stream.rest().await;
    assert!(
        rest.iter().all(|event| event.event == "heartbeat"),
        "{rest:?}"
    );
    let status = world.service.wait_exit(WAIT).await;
    assert!(status.success(), "{status}");

    let mut again = world
        .open(
            &other.base_url,
            &owner,
            &id,
            &run,
            Some(&first_id.to_string()),
        )
        .await;
    let events_after = again.rest().await;
    let kinds: Vec<&str> = events_after
        .iter()
        .map(|event| event.event.as_str())
        .collect();
    assert_eq!(kinds, ["heartbeat", "end"]);
    let end = events_after.last().expect("the end");
    assert_eq!(end.json()["status"], "interrupted");
    // Of this run only: the tests share the database.
    let mut admin = world.product.superuser().await;
    let partials: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM assistant.answer_partials WHERE run_id = $1::uuid",
    )
    .bind(&run)
    .fetch_one(&mut admin)
    .await
    .expect("count the partial answers");
    assert_eq!(
        partials, 0,
        "the interrupted run's end deleted its partial answer"
    );
    drop(sender);
    println!("the restart mid-answer read as interrupted on the other replica");
}

/// The stored prompt version and tool names of conversation `id`, read as the superuser.
async fn stored_preamble(world: &World, id: &str) -> (String, Vec<String>) {
    let mut admin = world.product.superuser().await;
    let (version, tools): (String, String) = sqlx::query_as(
        "SELECT prompt_version, tools FROM assistant.conversations \
         WHERE conversation_id = $1::uuid",
    )
    .bind(id)
    .fetch_one(&mut admin)
    .await
    .expect("read the stored preamble");
    let tools: Value = serde_json::from_str(&tools).expect("the tools are JSON");
    let names = tools
        .as_array()
        .expect("a list of tools")
        .iter()
        .map(|tool| tool["name"].as_str().expect("a name").to_owned())
        .collect();
    (version, names)
}

/// The tools the model was offered in `request`, by name.
fn offered(request: &Recorded) -> Vec<String> {
    request.json()["tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .map(|tool| tool["name"].as_str().expect("a name").to_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// With `streams.answer_tool` off (the emergency switch): a new conversation gets the prompt before the
/// answer tool (2026-11-03) and no answer tool, as the version before this one would start it;
/// nothing of the answer goes out while the question runs, and the stream gets the whole text
/// answer with the end.
#[tokio::test(flavor = "multi_thread")]
async fn with_the_switch_off_a_new_conversation_answers_as_text_after_the_question() {
    let Some(world) = world().await else {
        return;
    };
    let owner = User::new(TenantId::generate(), TenantRole::Viewer);
    let id = world.conversation(&owner).await;
    let (version, tools) = stored_preamble(&world, &id).await;
    assert_eq!(version, "2026-11-03");
    assert!(!tools.iter().any(|tool| tool == ANSWER_TOOL), "{tools:?}");
    let run = world
        .asked(&owner, &id, &format!("Did floor 3 report? {HOLD}"))
        .await;
    world.gateway.wait_calls(1).await;
    let request = &world.gateway.stand_in.requests()[0];
    assert!(!offered(request).iter().any(|tool| tool == ANSWER_TOOL));
    assert!(!request.body.contains("the answer tool"));

    let mut stream = world
        .open(&world.service.base_url, &owner, &id, &run, None)
        .await;
    let first = stream.next().await.expect("the first frame");
    assert_eq!(first.event, "heartbeat");
    let mut admin = world.product.superuser().await;
    let partials: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM assistant.answer_partials WHERE run_id = $1::uuid",
    )
    .bind(&run)
    .fetch_one(&mut admin)
    .await
    .expect("count the partial answers");
    assert_eq!(partials, 0, "nothing of a text answer is written early");

    world.gateway.open(1);
    let answered = world.wait_run(&owner, &id, &run, "answered").await;
    let stored = shown_answer(&answered);
    let events = stream.rest().await;
    let (text, _) = joined(&events, 0);
    assert_eq!(text, stored);
    let end = events.last().expect("the end");
    assert_eq!(end.json()["status"], "answered");
    println!("with the switch off, a new conversation got prompt {version} and no answer tool");
}

/// A service started without `TRUSTIOT__STREAMS__ANSWER_TOOL` (the default is on, the user's
/// decision of 2026-11-06): a new conversation gets the 2026-11-05 prompt and the answer tool,
/// and its model request carries the tool.
#[tokio::test(flavor = "multi_thread")]
async fn without_the_switch_a_new_conversation_has_the_answer_tool_and_the_current_prompt() {
    let Some(world) = world_with(|_, _| ok(fixture("answer_end_turn.json")), &[]).await else {
        return;
    };
    let owner = User::new(TenantId::generate(), TenantRole::Viewer);
    let id = world.conversation(&owner).await;
    let (version, tools) = stored_preamble(&world, &id).await;
    assert_eq!(version, "2026-11-05");
    assert!(tools.iter().any(|tool| tool == ANSWER_TOOL), "{tools:?}");
    let run = world.asked(&owner, &id, "Did floor 3 report?").await;
    world.wait_run(&owner, &id, &run, "answered").await;
    let request = &world.gateway.stand_in.requests()[0];
    assert!(offered(request).iter().any(|tool| tool == ANSWER_TOOL));
    assert!(request.body.contains("the answer tool"));
    println!("without the switch, a new conversation got prompt {version} and the answer tool");
}

/// A conversation started while the switch was on keeps the answer tool on a replica whose
/// switch is off (turned off later, or a replica not yet switched): the call is the answer, its
/// paragraphs stream while the question runs, and the answer is stored. A conversation started
/// on that replica has no answer tool.
#[tokio::test(flavor = "multi_thread")]
async fn a_conversation_started_with_the_answer_tool_keeps_it_on_a_replica_with_the_switch_off() {
    let Some((world, script)) = scripted_world().await else {
        return;
    };
    let off = world.replica_with(&[(ANSWER_TOOL_SWITCH, "false")]).await;
    let owner = User::new(TenantId::generate(), TenantRole::Viewer);
    let id = world.conversation(&owner).await;
    let (version, tools) = stored_preamble(&world, &id).await;
    assert_eq!(version, "2026-11-05");
    assert!(tools.iter().any(|tool| tool == ANSWER_TOOL), "{tools:?}");
    let without = world.conversation_on(&off, &owner).await;
    let (version, tools) = stored_preamble(&world, &without).await;
    assert_eq!(version, "2026-11-03", "the replica's switch is off");
    assert!(!tools.iter().any(|tool| tool == ANSWER_TOOL), "{tools:?}");

    let (sender, reply) = support::events_reply();
    script.lock().expect("the lock").push_back(reply);
    let text = format!("{TOOL_FIRST}\n\n{}\n\n{TOOL_THIRD}", tool_second());
    let (events, pieces) = answer_tool_events(&text);
    let cut = events_through(&events, &pieces, "\\n\\nThe rise");
    let run = world
        .asked_on(&off, &owner, &id, "How much did floor 3 use?")
        .await;
    for event in &events[..cut] {
        sender.send(event.clone()).await.expect("the gateway reads");
    }
    let mut stream = world.open(&off.base_url, &owner, &id, &run, None).await;
    let first_id = text_until(&mut stream, 0, TOOL_FIRST).await;
    let detail = world.read(&owner, &id).await;
    assert_eq!(
        detail["latest_run"]["status"], "running",
        "it went out early"
    );
    for event in &events[cut..] {
        sender.send(event.clone()).await.expect("the gateway reads");
    }
    drop(sender);
    let answered = world.wait_run(&owner, &id, &run, "answered").await;
    let stored = shown_answer(&answered);
    assert!(stored.starts_with(TOOL_FIRST), "{stored:?}");
    assert!(stored.ends_with(TOOL_THIRD), "{stored:?}");
    let rest = stream.rest().await;
    let (tail, _) = joined(&rest, first_id);
    assert_eq!(format!("{TOOL_FIRST}{tail}"), stored);
    assert_eq!(rest.last().expect("the end").json()["status"], "answered");

    let request = world
        .gateway
        .stand_in
        .requests()
        .into_iter()
        .last()
        .expect("the answer's call");
    let tools = request.json()["tools"].clone();
    let answer_tool = tools
        .as_array()
        .expect("tools")
        .iter()
        .find(|tool| tool["name"] == ANSWER_TOOL)
        .expect("the answer tool is still offered")
        .clone();
    assert_eq!(answer_tool["eager_input_streaming"], true);
    println!(
        "a conversation started with the answer tool streamed its paragraphs on a replica with the switch off"
    );
}
