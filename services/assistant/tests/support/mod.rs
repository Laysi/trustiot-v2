//! Test support: a stand-in AI gateway on a loopback socket that answers with recorded
//! responses (tests/fixtures) and keeps every request it got, the agent wired to it, fake
//! tools, and a log capture. No real gateway, provider or key.

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
use serde_json::Value;
use serde_json::value::RawValue;
use tokio::net::TcpListener;
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
    /// HTTP status and JSON body.
    Json(u16, String),
    /// Never answers.
    Never,
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
    match (shared.responder)(recorded, index).await {
        Reply::Json(status, body) => (
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

/// Settings with a budget no test reaches unless it sets one.
pub fn settings() -> AgentSettings {
    AgentSettings {
        effort: Effort::Medium,
        cache_ttl: CacheTtl::FiveMinutes,
        max_output_tokens: 16_000,
        call_timeout: WAIT,
        budget_tokens: 100_000_000,
        wind_down_percent: 80,
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
