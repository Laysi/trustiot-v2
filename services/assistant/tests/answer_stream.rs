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

/// The service whose model answers every call with the recorded answer.
async fn world() -> Option<World> {
    world_with(|_, _| ok(fixture("answer_end_turn.json")), &[]).await
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

/// The limits: a user holds at most `streams.max_per_user` streams on a replica (429), a replica
/// at most `streams.max_per_replica` (503 with `Retry-After`); a closed stream gives its slot
/// back, and a refused request is logged without the conversation's content.
#[tokio::test(flavor = "multi_thread")]
async fn a_replica_and_a_user_hold_at_most_their_streams() {
    let Some(world) = world_with(
        |_, _| ok(fixture("answer_end_turn.json")),
        &[
            ("TRUSTIOT__STREAMS__MAX_PER_USER", "1"),
            ("TRUSTIOT__STREAMS__MAX_PER_REPLICA", "2"),
            ("TRUSTIOT__STREAMS__HEARTBEAT_INTERVAL", FAST_HEARTBEAT),
        ],
    )
    .await
    else {
        return;
    };
    let tenant = TenantId::generate();
    let (alice, bob, carol) = (
        User::new(tenant, TenantRole::Viewer),
        User::new(tenant, TenantRole::Viewer),
        User::new(tenant, TenantRole::Viewer),
    );
    let mut runs = Vec::new();
    for user in [&alice, &bob, &carol] {
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
    world
        .refused(&world.service, &alice, &runs[0].0, &runs[0].1, None)
        .await
        .expect_error(429, "rate_limited", "stream.limited");
    let mut bobs = world.open(&base, &bob, &runs[1].0, &runs[1].1, None).await;
    let busy = world
        .refused(&world.service, &carol, &runs[2].0, &runs[2].1, None)
        .await;
    busy.expect_error(503, "unavailable", "stream.busy");
    assert_eq!(busy.header("retry-after"), Some("5"));
    let logged = world
        .service
        .wait_logged("answer stream refused: the replica holds as many streams as it may")
        .await;
    assert_eq!(logged["fields"]["max"], 2);

    drop(alices);
    let what = "the closed stream's slot given back";
    service::wait_for(what, WAIT, || async {
        let reply = world.refused_head(&carol, &runs[2].0, &runs[2].1).await;
        (reply == 200).then_some(())
    })
    .await;
    assert_eq!(
        bobs.next().await.expect("bob's stream is open").event,
        "heartbeat"
    );
    world.gateway.open(3);
    let logs = world.logs(&world.service);
    assert!(!logs.contains(HOLD), "no question in the logs");
    println!("one stream per user, two per replica; a closed stream's slot came back");
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
