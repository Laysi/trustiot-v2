//! Test support: a stand-in AI gateway on a loopback socket that answers with recorded
//! responses (tests/fixtures) and keeps every request it got, the agent wired to it, fake
//! tools, and a log capture. No real gateway, provider or key.
//!
//! The agent asks for every response as a stream (`"stream": true`): the stand-in turns a
//! [`Reply::Json`] message into the server-sent events the Messages API sends for it
//! ([`sse_events`]), with each text, thinking and tool input in small pieces, so every test runs
//! the streaming path. [`Reply::JsonBody`] answers with the JSON as it is (a gateway that does not
//! stream), and [`Reply::Events`] sends events as a test hands them over.

#![allow(
    dead_code,
    reason = "each test binary uses a different part of the support module"
)]

use std::collections::BTreeMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use assistant::history::Principal;
use assistant::model::anthropic::{AnthropicGateway, TENANT_HEADER};
use assistant::model::{CacheTtl, Effort};
use assistant::tools::{
    Tool, ToolContext, ToolError, ToolLimits, ToolOutput, ToolRegistry, ToolSpec,
};
use assistant::{Agent, AgentSettings};
use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use contract::{TenantId, UserId};
use serde_json::value::RawValue;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// How long a test waits for something it caused: a bound only, far above what any of it
/// takes, so a loaded machine does not fail the test (CLAUDE.md, 「Test time limits」).
pub const WAIT: Duration = Duration::from_secs(120);

pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// A recorded fixture.
pub fn fixture(name: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read fixture {}: {error}", path.display()))
}

/// `step_tool_use.json` for step `step`.
pub fn tool_step(step: usize) -> String {
    fixture("step_tool_use.json").replace("__STEP__", &format!("{step:04}"))
}

/// A model step that calls `calls`, made from the recorded step: its thinking, then these tool
/// calls.
pub fn step(index: usize, calls: &[(&str, Value)]) -> String {
    let mut message: Value = serde_json::from_str(&tool_step(index)).expect("the fixture is JSON");
    let content = message["content"].as_array_mut().expect("content");
    content.retain(|block| block["type"] != "tool_use");
    for (call, (name, input)) in calls.iter().enumerate() {
        content.push(json!({
            "type": "tool_use",
            "id": format!("toolu_{index:04}_{call}"),
            "name": name,
            "input": input,
        }));
    }
    message.to_string()
}

/// One tool result: whether it is an error, and its text.
#[derive(Debug)]
pub struct ToolResult {
    pub is_error: bool,
    pub text: String,
}

/// The tool results of the last message of `request`.
pub fn tool_results(request: &Recorded) -> Vec<ToolResult> {
    let body = request.json();
    let last = body["messages"]
        .as_array()
        .and_then(|messages| messages.last())
        .cloned()
        .expect("a message");
    last["content"]
        .as_array()
        .expect("blocks")
        .iter()
        .filter(|block| block["type"] == "tool_result")
        .map(|block| ToolResult {
            is_error: block["is_error"].as_bool().unwrap_or(false),
            text: block["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|part| part["text"].as_str())
                .collect(),
        })
        .collect()
}

/// What the stand-in got.
#[derive(Clone, Debug)]
pub struct Recorded {
    pub path: String,
    pub tenant: Option<String>,
    pub body: String,
}

impl Recorded {
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.body).expect("the request body is JSON")
    }

    /// The `messages` as the JSON text sent, one per message.
    pub fn raw_messages(&self) -> Vec<String> {
        #[derive(serde::Deserialize)]
        struct Messages<'a> {
            #[serde(borrow)]
            messages: Vec<&'a RawValue>,
        }
        let parsed: Messages<'_> =
            serde_json::from_str(&self.body).expect("the request body has messages");
        parsed
            .messages
            .iter()
            .map(|raw| raw.get().to_owned())
            .collect()
    }

    /// The role of the last message.
    pub fn last_role(&self) -> String {
        let body = self.json();
        let messages = body["messages"].as_array().expect("messages");
        messages
            .last()
            .and_then(|message| message["role"].as_str())
            .unwrap_or_default()
            .to_owned()
    }

    /// Whether any message's text contains `needle`.
    pub fn mentions(&self, needle: &str) -> bool {
        self.json()["messages"].to_string().contains(needle)
    }
}

/// The stand-in's answer.
pub enum Reply {
    /// HTTP status and JSON body; a 200 to a streaming request goes out as its events
    /// ([`sse_events`]).
    Json(u16, String),
    /// HTTP status and JSON body, as they are, even to a streaming request.
    JsonBody(u16, String),
    /// A 200 event stream whose bytes are what the receiver gets, in order; it ends when the
    /// sender is dropped.
    Events(mpsc::Receiver<String>),
    /// Never answers.
    Never,
}

/// The size of the pieces [`sse_events`] cuts texts and tool inputs into, in characters: small,
/// so an escape or a character is often cut.
pub const PIECE_CHARS: usize = 5;

/// The events of the Messages API's stream of `message` (a JSON message): `message_start`, each
/// block's start, deltas in pieces of [`PIECE_CHARS`] characters and stop, `message_delta`
/// with the stop reason and usage, and `message_stop`. Each item is one event's text.
pub fn sse_events(message: &str) -> Vec<String> {
    let message: Value = serde_json::from_str(message).expect("the message is JSON");
    let mut start = message.clone();
    start["content"] = json!([]);
    start["stop_reason"] = Value::Null;
    start["stop_sequence"] = Value::Null;
    let mut events = vec![event(
        "message_start",
        &json!({"type": "message_start", "message": start}),
    )];
    let blocks = message["content"].as_array().cloned().unwrap_or_default();
    for (index, block) in blocks.iter().enumerate() {
        let mut empty = block.clone();
        let mut deltas = Vec::new();
        match block["type"].as_str() {
            Some("text") => {
                empty["text"] = json!("");
                for piece in pieces(block["text"].as_str().unwrap_or_default()) {
                    deltas.push(json!({"type": "text_delta", "text": piece}));
                }
            }
            Some("thinking") => {
                empty["thinking"] = json!("");
                empty["signature"] = json!("");
                for piece in pieces(block["thinking"].as_str().unwrap_or_default()) {
                    deltas.push(json!({"type": "thinking_delta", "thinking": piece}));
                }
                deltas.push(json!({
                    "type": "signature_delta",
                    "signature": block["signature"].as_str().unwrap_or_default(),
                }));
            }
            Some("tool_use") => {
                empty["input"] = json!({});
                let input = block["input"].to_string();
                for piece in pieces(&input) {
                    deltas.push(json!({"type": "input_json_delta", "partial_json": piece}));
                }
            }
            _ => {}
        }
        events.push(event(
            "content_block_start",
            &json!({"type": "content_block_start", "index": index, "content_block": empty}),
        ));
        for delta in deltas {
            events.push(event(
                "content_block_delta",
                &json!({"type": "content_block_delta", "index": index, "delta": delta}),
            ));
        }
        events.push(event(
            "content_block_stop",
            &json!({"type": "content_block_stop", "index": index}),
        ));
    }
    events.push(event(
        "message_delta",
        &json!({
            "type": "message_delta",
            "delta": {
                "stop_reason": message["stop_reason"],
                "stop_sequence": message["stop_sequence"],
            },
            "usage": message["usage"],
        }),
    ));
    events.push(event("message_stop", &json!({"type": "message_stop"})));
    events
}

/// One server-sent event.
pub fn event(name: &str, data: &Value) -> String {
    format!("event: {name}\ndata: {data}\n\n")
}

/// `text` in pieces of [`PIECE_CHARS`] characters.
pub fn pieces(text: &str) -> Vec<String> {
    let characters: Vec<char> = text.chars().collect();
    characters
        .chunks(PIECE_CHARS)
        .map(|chunk| chunk.iter().collect())
        .collect()
}

/// A model step whose first tool call is the answer tool with `text`, after the recorded step's
/// thinking: the answer of the question.
pub fn answer_step(index: usize, text: &str) -> String {
    step(index, &[("answer", json!({"text": text}))])
}

/// The events of a model turn: `blocks` in order, each a block's start (with its empty form)
/// and its deltas, then the stop reason. Each delta is sent as its own event.
pub fn turn_events(blocks: &[(Value, Vec<Value>)], stop_reason: &str) -> Vec<String> {
    let mut events = vec![event(
        "message_start",
        &json!({"type": "message_start", "message": {
            "id": "msg_stream", "type": "message", "role": "assistant", "content": [],
            "model": "claude-opus-5-5", "stop_reason": null, "stop_sequence": null,
            "usage": {"input_tokens": 300, "cache_creation_input_tokens": 0,
                      "cache_read_input_tokens": 4096, "output_tokens": 1},
        }}),
    )];
    for (index, (start, deltas)) in blocks.iter().enumerate() {
        events.push(event(
            "content_block_start",
            &json!({"type": "content_block_start", "index": index, "content_block": start}),
        ));
        for delta in deltas {
            events.push(event(
                "content_block_delta",
                &json!({"type": "content_block_delta", "index": index, "delta": delta}),
            ));
        }
        events.push(event(
            "content_block_stop",
            &json!({"type": "content_block_stop", "index": index}),
        ));
    }
    events.push(event(
        "message_delta",
        &json!({"type": "message_delta", "delta": {"stop_reason": stop_reason,
                 "stop_sequence": null}, "usage": {"output_tokens": 200}}),
    ));
    events.push(event("message_stop", &json!({"type": "message_stop"})));
    events
}

pub fn thinking_block() -> (Value, Vec<Value>) {
    (
        json!({"type": "thinking", "thinking": "", "signature": ""}),
        vec![
            json!({"type": "thinking_delta", "thinking": "The counts are in; writing it up."}),
            json!({"type": "signature_delta", "signature": "EqMBCkYICxIManswer"}),
        ],
    )
}

pub fn text_block(text: &str) -> (Value, Vec<Value>) {
    (
        json!({"type": "text", "text": ""}),
        vec![json!({"type": "text_delta", "text": text})],
    )
}

/// A tool call whose input arrives as `pieces` of JSON.
pub fn call_block(id: &str, name: &str, pieces: &[&str]) -> (Value, Vec<Value>) {
    (
        json!({"type": "tool_use", "id": id, "name": name, "input": {}}),
        pieces
            .iter()
            .map(|piece| json!({"type": "input_json_delta", "partial_json": piece}))
            .collect(),
    )
}

/// The JSON of `{"text": text}` in pieces of `size` bytes, cut between characters.
pub fn json_pieces(text: &str, size: usize) -> Vec<String> {
    let json = json!({ "text": text }).to_string();
    let mut pieces = Vec::new();
    let mut piece = String::new();
    for character in json.chars() {
        piece.push(character);
        if piece.len() >= size {
            pieces.push(std::mem::take(&mut piece));
        }
    }
    if !piece.is_empty() {
        pieces.push(piece);
    }
    pieces
}

/// A sender for [`Reply::Events`] and the reply.
pub fn events_reply() -> (mpsc::Sender<String>, Reply) {
    let (sender, receiver) = mpsc::channel(64);
    (sender, Reply::Events(receiver))
}

/// Decides the answer to a request; gets the request and how many came before it.
pub type Responder = Arc<dyn Fn(Recorded, usize) -> BoxFuture<Reply> + Send + Sync>;

struct Shared {
    requests: Mutex<Vec<Recorded>>,
    responder: Responder,
}

/// The stand-in AI gateway.
pub struct StandIn {
    pub url: String,
    shared: Arc<Shared>,
    server: JoinHandle<()>,
}

impl Drop for StandIn {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl StandIn {
    pub async fn start(responder: Responder) -> Self {
        let shared = Arc::new(Shared {
            requests: Mutex::new(Vec::new()),
            responder,
        });
        let router = Router::new()
            .route("/internal/roles/{role}/v1/messages", post(messages))
            .with_state(Arc::clone(&shared));
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a loopback port");
        let address = listener.local_addr().expect("the local address");
        let server = tokio::spawn(async move {
            axum::serve(listener, router)
                .await
                .expect("serve the stand-in gateway");
        });
        Self {
            url: format!("http://{address}"),
            shared,
            server,
        }
    }

    /// Every request so far, in arrival order.
    pub fn requests(&self) -> Vec<Recorded> {
        self.shared.requests.lock().expect("the lock").clone()
    }
}

async fn messages(
    State(shared): State<Arc<Shared>>,
    Path(role): Path<String>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let recorded = Recorded {
        path: format!("/internal/roles/{role}/v1/messages"),
        tenant: headers
            .get(TENANT_HEADER)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned),
        body,
    };
    let index = {
        let mut requests = shared.requests.lock().expect("the lock");
        requests.push(recorded.clone());
        requests.len() - 1
    };
    let streaming = serde_json::from_str::<Value>(&recorded.body)
        .is_ok_and(|body| body["stream"] == json!(true));
    match (shared.responder)(recorded, index).await {
        Reply::Json(200, body) if streaming => {
            let (sender, receiver) = mpsc::channel(1);
            tokio::spawn(async move {
                for event in sse_events(&body) {
                    if sender.send(event).await.is_err() {
                        return;
                    }
                }
            });
            event_stream(receiver)
        }
        Reply::Events(receiver) => event_stream(receiver),
        Reply::Json(status, body) | Reply::JsonBody(status, body) => (
            StatusCode::from_u16(status).expect("a valid status"),
            [
                ("content-type", "application/json"),
                ("ai-gateway-model", "anthropic/claude-opus-5-5"),
            ],
            body,
        )
            .into_response(),
        Reply::Never => std::future::pending().await,
    }
}

/// A 200 event stream of what `receiver` gets.
fn event_stream(receiver: mpsc::Receiver<String>) -> Response {
    let body = axum::body::Body::from_stream(futures_util::stream::unfold(
        receiver,
        |mut receiver| async move {
            receiver
                .recv()
                .await
                .map(|event| (Ok::<_, std::convert::Infallible>(event), receiver))
        },
    ));
    (
        StatusCode::OK,
        [
            ("content-type", "text/event-stream"),
            ("ai-gateway-model", "anthropic/claude-opus-5-5"),
        ],
        body,
    )
        .into_response()
}

/// A responder from a plain function.
pub fn respond(reply: impl Fn(&Recorded, usize) -> Reply + Send + Sync + 'static) -> Responder {
    let reply = Arc::new(reply);
    Arc::new(move |request: Recorded, index: usize| -> BoxFuture<Reply> {
        let reply = Arc::clone(&reply);
        Box::pin(async move { reply(&request, index) })
    })
}

pub fn ok(body: String) -> Reply {
    Reply::Json(200, body)
}

/// Settings with a budget no test reaches unless it sets one, and the answer tool in new
/// conversations (`streams.answer_tool` on), which the answer tool's tests need; a test of the
/// default sets `answer_tool: false`.
pub fn settings() -> AgentSettings {
    AgentSettings {
        effort: Effort::Medium,
        cache_ttl: CacheTtl::FiveMinutes,
        max_output_tokens: 16_000,
        call_timeout: WAIT,
        budget_tokens: 100_000_000,
        wind_down_percent: 80,
        answer_tool: true,
    }
}

pub fn limits() -> ToolLimits {
    ToolLimits {
        call_timeout: WAIT,
        max_calls_per_step: 8,
        max_text_bytes: 64 << 10,
        max_images: 4,
        max_image_bytes: 5 << 20,
    }
}

/// The agent, calling the stand-in, with `tools`.
pub fn agent(stand_in: &StandIn, tools: Vec<Arc<dyn Tool>>, settings: AgentSettings) -> Agent {
    let gateway =
        AnthropicGateway::new(&stand_in.url, "main", 32 << 20).expect("a valid gateway URL");
    let registry = ToolRegistry::new(tools, limits()).expect("a valid tool set");
    Agent::new(Arc::new(gateway), registry, settings)
}

pub fn principal() -> Principal {
    Principal {
        tenant_id: TenantId::generate(),
        user_id: UserId::generate(),
    }
}

/// The tool the recorded steps call: answers with a count.
pub struct CountReadings;

impl Tool for CountReadings {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "count_readings".into(),
            description: "Counts a meter's readings over the last hours.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "meter": {"type": "string"},
                    "hours": {"type": "integer", "minimum": 1, "maximum": 168}
                },
                "required": ["meter", "hours"]
            }),
        }
    }

    fn call(
        self: Arc<Self>,
        _context: ToolContext,
        input: Value,
    ) -> assistant::model::BoxFuture<'static, Result<ToolOutput, ToolError>> {
        Box::pin(async move {
            let Some(meter) = input["meter"].as_str() else {
                return Err(ToolError::Input(
                    "`meter` is required: the meter's name, such as floor-3-0001.".into(),
                ));
            };
            Ok(ToolOutput::text(format!("{meter}: 60 readings")))
        })
    }
}

/// A log event: its message and fields, as text.
#[derive(Clone, Debug)]
pub struct Event {
    pub target: String,
    pub level: tracing::Level,
    pub message: String,
    pub fields: BTreeMap<String, String>,
}

/// Captures every event of the thread it is installed on. Another test's thread can hide the
/// events (Laysi/trustiot-v2#581): tests/agent_logs.rs runs its tests one at a time, and
/// [`Capture::install`] works while other tests run in parallel.
#[derive(Clone, Default)]
pub struct Capture(Arc<Mutex<Vec<Event>>>);

impl Capture {
    /// Captures the events of the current thread until the guard is dropped.
    ///
    /// tracing caches per callsite whether any subscriber wants its events. While only one
    /// dispatcher exists in the process, tracing-core computes that from the default of the
    /// thread that reaches the callsite first, so a callsite another test thread (with no
    /// subscriber) reached first is cached as "never" for every thread, and this capture misses
    /// its events (seen on 2026-10-23: no `tool call completed` while the tests ran in
    /// parallel). A second dispatcher kept alive for the whole test binary makes tracing-core
    /// ask every live dispatcher instead.
    pub fn install() -> (Self, tracing::subscriber::DefaultGuard) {
        use tracing_subscriber::layer::SubscriberExt as _;
        static KEEPER: OnceLock<tracing::Dispatch> = OnceLock::new();
        KEEPER.get_or_init(|| tracing::Dispatch::new(tracing::subscriber::NoSubscriber::new()));
        let capture = Self::default();
        let guard =
            tracing::subscriber::set_default(tracing_subscriber::registry().with(capture.clone()));
        (capture, guard)
    }

    pub fn events(&self) -> Vec<Event> {
        self.0.lock().expect("the lock").clone()
    }
}

struct Visitor<'a>(&'a mut BTreeMap<String, String>);

impl tracing::field::Visit for Visitor<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0.insert(field.name().to_owned(), format!("{value:?}"));
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.0.insert(field.name().to_owned(), value.to_owned());
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Capture {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _context: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut fields = BTreeMap::new();
        event.record(&mut Visitor(&mut fields));
        let message = fields.remove("message").unwrap_or_default();
        self.0.lock().expect("the lock").push(Event {
            target: event.metadata().target().to_owned(),
            level: *event.metadata().level(),
            message,
            fields,
        });
    }
}
