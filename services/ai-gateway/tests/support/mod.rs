//! Test support: the scripted provider servers and test CA of crates/ai-clients (included by
//! path), its recorded fixtures, configuration from variables as service-kit reads them,
//! requests to the gateway's routes without a socket, and a throwaway TLS Postgres with the
//! gateway's schema (`database`, on service-kit's test support, included by path). No real
//! provider and no real key: the key is made at run time (`upstream::key_text`).

#![allow(
    dead_code,
    unused_imports,
    reason = "each test binary uses a different part of the support module"
)]

use std::ffi::OsString;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use ai_clients::Tls;
use ai_gateway::{Config, ENV_PREFIX, Gateway, Store, TENANT_HEADER, routes};
use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use bytes::Bytes;
use tower::ServiceExt;

#[path = "../../../../crates/ai-clients/tests/support/mod.rs"]
pub mod upstream;

pub use upstream::{Server, Step, http_response, key_text, sse_head, sse_without_comments};

#[path = "../../../../crates/service-kit/tests/support/postgres.rs"]
pub mod postgres;
#[path = "../../../../crates/service-kit/tests/support/start.rs"]
pub mod start;

pub mod database;

/// The database settings of a gateway whose tests never reach the database: a closed loopback
/// port. Routers built by [`Vars::router`] have no store, so these are only read, never used.
const NO_DATABASE: [(&str, &str); 7] = [
    ("AI_GATEWAY_POSTGRES_HOST", "127.0.0.1"),
    ("AI_GATEWAY_POSTGRES_PORT", "9"),
    ("AI_GATEWAY_POSTGRES_DATABASE", "none"),
    ("AI_GATEWAY_POSTGRES_SSLMODE", "verify-full"),
    ("AI_GATEWAY_POSTGRES_CA_FILE", "/nonexistent/ca.pem"),
    ("AI_GATEWAY__DATABASE__USERNAME", "ai_gateway_app"),
    ("AI_GATEWAY__DATABASE__PASSWORD", "unused"),
];

/// `http.request_timeout` for routers built here: long enough that no role's limits hit it.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(3600);

/// How long a test waits for the gateway's answer or the end of its stream: a bound only, far
/// above the longest time limit a test configures (fallback.rs, `TIMEOUT_UNDER_TEST`).
pub const WAIT: Duration = Duration::from_secs(180);

/// The tenant every test request names.
pub const TENANT: &str = "tenant-0001";

pub const MESSAGES: &str = "/internal/roles/main/v1/messages";
pub const DECISIONS: &str = "/internal/roles/router/v1/decisions";

/// A fixture of crates/ai-clients (`anthropic/…`, `decisions/…`).
pub fn fixture(name: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../crates/ai-clients/tests/fixtures")
        .join(name);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read fixture {}: {error}", path.display()))
}

/// The entry of `anthropic/errors.json` for `situation`: status, `retry-after`, body.
pub fn recorded_error(situation: &str) -> (u16, Option<String>, String) {
    let errors: serde_json::Value =
        serde_json::from_str(&fixture("anthropic/errors.json")).expect("errors.json is JSON");
    let entry = errors
        .as_array()
        .expect("a list")
        .iter()
        .find(|entry| entry["situation"] == situation)
        .unwrap_or_else(|| panic!("no recorded error {situation:?}"));
    let status = u16::try_from(entry["status"].as_u64().expect("a status")).expect("a u16");
    let retry_after = entry["retry_after"].as_str().map(str::to_owned);
    (status, retry_after, entry["body"].to_string())
}

/// An Anthropic error response from `anthropic/errors.json`.
pub fn anthropic_error(situation: &str) -> Vec<u8> {
    let (status, retry_after, body) = recorded_error(situation);
    let mut headers = vec![("content-type", "application/json")];
    if let Some(retry_after) = retry_after.as_deref() {
        headers.push(("retry-after", retry_after));
    }
    http_response(status, &headers, body.as_bytes())
}

/// A JSON response with a request ID.
pub fn json_reply(body: &str, request_id_header: &str, request_id: &str) -> Vec<u8> {
    http_response(
        200,
        &[
            ("content-type", "application/json"),
            (request_id_header, request_id),
        ],
        body.as_bytes(),
    )
}

/// A streaming response: `sse_head()` and the events of `text`.
pub fn stream_reply(text: &str) -> Vec<u8> {
    let mut out = sse_head();
    out.extend_from_slice(text.as_bytes());
    out
}

/// Configuration variables, built up by the helpers below; a later value of a variable
/// replaces the earlier one.
#[derive(Debug, Clone)]
pub struct Vars(Vec<(String, String)>);

impl Vars {
    /// Database settings that point nowhere ([`NO_DATABASE`]) and nothing else.
    pub fn new() -> Self {
        NO_DATABASE
            .iter()
            .fold(Self(Vec::new()), |vars, (name, value)| {
                vars.set(name, *value)
            })
    }

    pub fn set(mut self, name: &str, value: impl Into<String>) -> Self {
        self.0.retain(|(existing, _)| existing != name);
        self.0.push((name.to_owned(), value.into()));
        self
    }

    /// The settings of `database`.
    pub fn database(self, database: &database::Database) -> Self {
        database
            .vars()
            .into_iter()
            .fold(self, |vars, (name, value)| vars.set(name, value))
    }

    /// The variables, for a process.
    pub fn pairs(&self) -> &[(String, String)] {
        &self.0
    }

    /// A provider `name` speaking `protocol` at `server`, its key in `<NAME>_TEST_KEY`.
    pub fn provider(self, name: &str, protocol: &str, server: &Server) -> Self {
        let upper = name.to_ascii_uppercase();
        self.set(
            &format!("AI_GATEWAY__PROVIDERS__{upper}__PROTOCOL"),
            protocol,
        )
        .set(
            &format!("AI_GATEWAY__PROVIDERS__{upper}__KEY_ENV"),
            format!("{upper}_TEST_KEY"),
        )
        .set(
            &format!("AI_GATEWAY__PROVIDERS__{upper}__BASE_URL"),
            format!("http://{}", server.addr),
        )
    }

    /// A role with `models` (comma-separated `<provider>/<model>`).
    pub fn role(self, name: &str, models: &str) -> Self {
        let upper = name.to_ascii_uppercase();
        self.set(&format!("AI_GATEWAY__ROLES__{upper}__MODELS"), models)
    }

    pub fn config(&self) -> Config {
        service_kit::config::load_from::<Config, _>(
            ENV_PREFIX,
            self.0
                .iter()
                .map(|(name, value)| (OsString::from(name), OsString::from(value))),
        )
        .expect("a valid configuration")
        .app
    }

    /// The gateway's routes, every provider key being [`key_text`].
    pub fn router(&self) -> Router {
        self.router_with_request_timeout(REQUEST_TIMEOUT)
    }

    /// [`Vars::router`] for a service whose `http.request_timeout` is `request_timeout`.
    pub fn router_with_request_timeout(&self, request_timeout: Duration) -> Router {
        let key = key_text();
        let gateway = Gateway::build(
            &self.config(),
            &|_| Some(key.clone()),
            &Tls::none(),
            request_timeout,
        )
        .expect("build the gateway");
        routes(Arc::new(gateway))
    }

    /// A gateway that takes its roles' models from its database, as the binary does
    /// (`ai_gateway::start`), every provider key being [`key_text`]; and its routes.
    pub fn live(&self) -> (Arc<Gateway>, Router) {
        let config = self.config();
        let key = key_text();
        let store = Store::connect_lazy(&config).expect("a verify-full pool");
        let gateway = Gateway::build(
            &config,
            &|_| Some(key.clone()),
            &Tls::none(),
            REQUEST_TIMEOUT,
        )
        .expect("build the gateway")
        .with_store(store, config.database.timeout);
        let gateway = Arc::new(gateway);
        (Arc::clone(&gateway), routes(gateway))
    }
}

/// A response as the caller sees it.
#[derive(Debug)]
pub struct Reply {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Bytes,
}

impl Reply {
    pub fn text(&self) -> &str {
        std::str::from_utf8(&self.body).expect("a UTF-8 body")
    }

    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).expect("a JSON body")
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(name)
            .map(|value| value.to_str().expect("a text header"))
    }
}

/// A POST to the router with the tenant header, `extra` headers and `body`.
pub async fn post(router: &Router, path: &str, extra: &[(&str, &str)], body: &str) -> Reply {
    let mut request = Request::post(path)
        .header("content-type", "application/json")
        .header(TENANT_HEADER, TENANT);
    for (name, value) in extra {
        request = request.header(*name, *value);
    }
    send(
        router,
        request
            .body(Body::from(body.to_owned()))
            .expect("a request"),
    )
    .await
}

/// Sends `request` and reads the whole response (a stream to its end).
pub async fn send(router: &Router, request: Request<Body>) -> Reply {
    let response = tokio::time::timeout(WAIT, router.clone().oneshot(request))
        .await
        .unwrap_or_else(|_| panic!("the gateway did not answer within {WAIT:?}"))
        .expect("the router is infallible");
    let status = response.status();
    let headers = response.headers().clone();
    let body = tokio::time::timeout(WAIT, axum::body::to_bytes(response.into_body(), 64 << 20))
        .await
        .unwrap_or_else(|_| panic!("the response body did not end within {WAIT:?}"))
        .expect("read the body");
    Reply {
        status,
        headers,
        body,
    }
}

/// `text` with the first `"model": …` value replaced, as a caller would have written it.
pub fn with_caller_model(text: &str, from: &str, to: &str) -> String {
    assert!(text.contains(from), "the text has no {from}");
    text.replacen(from, to, 1)
}

/// Fails the test in CI, where every prerequisite must be present; skips it elsewhere.
#[allow(
    clippy::print_stderr,
    reason = "tells the developer why a test did not run"
)]
pub fn unavailable(what: &str) {
    assert!(
        std::env::var_os("CI").is_none(),
        "{what} is required when CI is set"
    );
    eprintln!("skipped: {what} is not available (set CI to make this a failure)");
}

/// `podman` or `docker`, whichever works.
pub fn container_runtime() -> Option<&'static str> {
    ["podman", "docker"].into_iter().find(|runtime| {
        Command::new(runtime)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    })
}

/// Runs a container runtime command and returns its trimmed stdout.
pub fn runtime_output(runtime: &str, args: &[&str]) -> String {
    let output = Command::new(runtime)
        .args(args)
        .output()
        .unwrap_or_else(|error| panic!("{runtime} {args:?}: {error}"));
    assert!(
        output.status.success(),
        "{runtime} {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// A unique name for a container created by this test process.
pub fn unique_name(kind: &str) -> String {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    format!(
        "ai-gateway-test-{kind}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

/// A container that is removed when dropped.
pub struct Container {
    pub runtime: &'static str,
    pub name: String,
}

impl Container {
    /// The host address mapped to `port` inside the container.
    pub fn host_address(&self, port: &str) -> SocketAddr {
        let mapping = runtime_output(self.runtime, &["port", &self.name, port]);
        mapping
            .lines()
            .find_map(|line| line.trim().parse().ok())
            .unwrap_or_else(|| panic!("no host address for {port}: {mapping}"))
    }
}

impl Drop for Container {
    fn drop(&mut self) {
        let _ = Command::new(self.runtime)
            // `--volumes`: the image's anonymous volumes go with the container (Laysi/trustiot-v2#334).
            .args(["rm", "--force", "--volumes", &self.name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}
