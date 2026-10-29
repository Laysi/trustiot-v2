//! Platform administrators read and change the AI gateway's model roles through the identity
//! service, end to end (Laysi/trustiot-v2#608): the real binary on the product's schema, applied
//! by the migration Job (`testkit::product`), sessions turned into internal identity tokens by
//! the service's own ext_authz check, and a stand-in AI gateway admin API on loopback that
//! records every request it gets and answers like the gateway (or as a step scripts it).
//!
//! - Without a valid token 401, and every caller but a platform administrator's own request
//!   403, on all three routes; the stand-in gets nothing.
//! - Reads, changes and resets answer what the stand-in answered; the stand-in sees
//!   `ai-gateway-actor: user:<id>` built from the token, never the client's own header of that
//!   name, a cookie or a token; the body it gets has only `models` and `expected_version`.
//! - The gateway's refusals map to M1 (404, 409 with the current version, 422 with a key per
//!   code); any other non-2xx, an unreadable answer, a lost connection or a timeout is a 503,
//!   for a change with `ai_model_role.outcome_unknown`, and the stand-in got the write once.
//! - Cross-site requests and changes without a JSON `Content-Type` are refused before anything
//!   is sent; so are bodies the service can tell are wrong.
//! - Each change writes `identity.ai_model_role_changed` with the administrator as the actor,
//!   which the platform's operation log shows; when that write fails, the change still answers
//!   200 and an ERROR is logged.
//! - Without `TRUSTIOT_AI_GATEWAY_ADMIN_URL`, or with an address nothing listens on, the routes
//!   answer 503.
//! - While the stand-in hangs, the ext_authz check answers as usual, a fifth change is refused
//!   at once, and the hung ones answer 503 after the client's own time limit.
//! - The logs hold no cookie or token.
//!
//! Starts a throwaway Postgres with TLS (podman or docker); skipped without either unless `CI`
//! is set. One test: the binaries share one product database.

mod support;

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use ai_gateway_api as api;
use axum::Router;
use axum::body::Bytes;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use contract::identity::Actor as TokenActor;
use contract::{ServiceName, UserId};
use serde_json::{Value, json};
use sqlx::PgConnection;
use support::{
    KeyFiles, Service, TestKey, cookie, new_secret, request, request_with_body, request_within,
    signing_keys,
};
use testkit::identity::TestIssuer;
use testkit::postgres::TEST_PASSWORD;
use testkit::product::{ProductPostgres, product_postgres};

const APP: &str = "app.saas.trustiot.test";
const KID: &str = "2026-10-a";
const ROLES: &str = "/api/identity/v1/platform/ai/model-roles";
const TENANTS: &str = "/api/identity/v1/tenants";
const PLATFORM_LOG: &str = "/api/identity/v1/platform/event-log";
const EVENT_TYPE: &str = "identity.ai_model_role_changed";

#[allow(
    clippy::print_stderr,
    reason = "the test report quotes these lines (cargo test -- --nocapture)"
)]
fn evidence(line: impl std::fmt::Display) {
    eprintln!("evidence: {line}");
}

/// Generous bounds for a loaded host (CLAUDE.md, "Test time limits"): starting the binary, the
/// event log consumer's delivery of a committed event (it polls every second when idle), and a
/// stand-in that should have received what the test sent.
const READY_LIMIT: Duration = Duration::from_secs(60);
const EVENT_LIMIT: Duration = Duration::from_secs(60);
const RECEIVED_LIMIT: Duration = Duration::from_secs(60);
const POLL_EVERY: Duration = Duration::from_millis(100);
/// How long a test request may wait for an answer the service gives only after its own time
/// limit for a gateway call (`admin_client::CALL_TIMEOUT`, 10 s), with room for a loaded host.
const HUNG_ANSWER_LIMIT: Duration = Duration::from_secs(120);

const INSERT_USER: &str =
    "INSERT INTO identity.users (id, platform_admin, email) VALUES ($1::uuid, $2, NULL)";
const INSERT_SESSION: &str = "INSERT INTO identity.sessions
    (id, token_hash, user_id, active_tenant_id, expires_at)
VALUES (gen_random_uuid(), $1, $2::uuid, $3::uuid, now() + interval '1 hour')";
const INSERT_MEMBERSHIP: &str = "INSERT INTO identity.memberships (tenant_id, user_id, tenant_role)
VALUES ($1::uuid, $2::uuid, $3)";
/// The model role events in the outbox, in the order they were written: tenant, actor, payload.
const OUTBOX_EVENTS: &str = "SELECT tenant_id::text, envelope->'actor'->>'user_id',
    (envelope->'payload')::text
FROM identity.outbox_event WHERE event_type = 'identity.ai_model_role_changed'
ORDER BY tx_order, aggregate_seq";

// ---------------------------------------------------------------------------------------------
// The stand-in AI gateway admin API.
// ---------------------------------------------------------------------------------------------

/// A request the stand-in got.
#[derive(Clone, Debug)]
struct Received {
    method: Method,
    path: String,
    query: Option<String>,
    headers: Vec<(String, String)>,
    body: String,
}

impl Received {
    /// The names of the headers it got.
    fn names(&self) -> Vec<&str> {
        self.headers.iter().map(|(name, _)| name.as_str()).collect()
    }

    fn header(&self, name: &str) -> Vec<&str> {
        self.headers
            .iter()
            .filter(|(header, _)| header.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
            .collect()
    }
}

/// How the stand-in answers.
#[derive(Clone, Debug)]
enum Mode {
    /// Like the gateway: roles `main` (Anthropic Messages) and `router` (OpenAI Decisions).
    Emulate,
    /// This status and body, whatever the request.
    Answer(u16, String),
    /// Never.
    Hang,
}

#[derive(Clone, Debug)]
struct StandInRole {
    protocol: &'static str,
    configured: Vec<String>,
    stored: Option<Vec<String>>,
    version: i64,
    updated_by: Option<String>,
}

struct StandIn {
    received: Mutex<Vec<Received>>,
    mode: Mutex<Mode>,
    roles: Mutex<BTreeMap<String, StandInRole>>,
    generation: Mutex<i64>,
    /// Requests being answered right now (the hung ones).
    in_flight: AtomicUsize,
}

const PROVIDERS: [(&str, &str); 2] = [
    ("anthropic", "anthropic-messages"),
    ("openai", "openai-decisions"),
];

impl StandIn {
    fn new() -> Arc<Self> {
        let mut roles = BTreeMap::new();
        roles.insert(
            "main".to_owned(),
            StandInRole {
                protocol: "anthropic-messages",
                configured: vec!["anthropic/claude-opus-5-5".to_owned()],
                stored: None,
                version: 0,
                updated_by: None,
            },
        );
        roles.insert(
            "router".to_owned(),
            StandInRole {
                protocol: "openai-decisions",
                configured: vec!["openai/gpt-6-luna".to_owned()],
                stored: None,
                version: 0,
                updated_by: None,
            },
        );
        Arc::new(Self {
            received: Mutex::new(Vec::new()),
            mode: Mutex::new(Mode::Emulate),
            roles: Mutex::new(roles),
            generation: Mutex::new(0),
            in_flight: AtomicUsize::new(0),
        })
    }

    fn set_mode(&self, mode: Mode) {
        *self.mode.lock().expect("mode") = mode;
    }

    fn received(&self) -> Vec<Received> {
        self.received.lock().expect("received").clone()
    }

    fn count(&self) -> usize {
        self.received.lock().expect("received").len()
    }

    /// The requests received after the first `from`.
    fn since(&self, from: usize) -> Vec<Received> {
        self.received()[from..].to_vec()
    }

    fn role(&self, name: &str) -> StandInRole {
        self.roles.lock().expect("roles")[name].clone()
    }

    fn role_json(name: &str, role: &StandInRole) -> api::Role {
        api::Role {
            role: name.to_owned(),
            protocol: role.protocol.to_owned(),
            models: role
                .stored
                .clone()
                .unwrap_or_else(|| role.configured.clone()),
            configured_models: role.configured.clone(),
            source: if role.stored.is_some() {
                api::Source::Database
            } else {
                api::Source::Configuration
            },
            version: role.version,
            ignored: None,
            updated_at: role
                .updated_by
                .as_ref()
                .map(|_| "2026-10-29T08:00:00.000000Z".to_owned()),
            updated_by: role.updated_by.clone(),
        }
    }

    fn refuse(code: api::ErrorCode, current_version: Option<i64>) -> Response {
        let status = StatusCode::from_u16(code.status()).expect("a status");
        let body = api::ErrorBody {
            error: api::ErrorDetail {
                code,
                message: "refused".to_owned(),
                current_version,
            },
        };
        (status, axum::Json(body)).into_response()
    }

    /// The gateway's rules for a model list of a role with `protocol`.
    fn check_models(models: &[String], protocol: &str) -> Result<(), api::ErrorCode> {
        let mut seen = std::collections::BTreeSet::new();
        if !(1..=8).contains(&models.len()) || !models.iter().all(|m| seen.insert(m.clone())) {
            return Err(api::ErrorCode::InvalidModels);
        }
        for model in models {
            let Some((provider, name)) = model.split_once('/') else {
                return Err(api::ErrorCode::InvalidModels);
            };
            if name.is_empty() {
                return Err(api::ErrorCode::InvalidModels);
            }
            let Some((_, provider_protocol)) = PROVIDERS.iter().find(|(p, _)| *p == provider)
            else {
                return Err(api::ErrorCode::UnknownProvider);
            };
            if *provider_protocol != protocol {
                return Err(api::ErrorCode::ProtocolMismatch);
            }
        }
        Ok(())
    }

    fn emulate(&self, received: &Received) -> Response {
        let actor = received.header(api::ACTOR_HEADER);
        let [actor] = actor.as_slice() else {
            return Self::refuse(api::ErrorCode::InvalidActor, None);
        };
        if !api::is_valid_actor(actor) {
            return Self::refuse(api::ErrorCode::InvalidActor, None);
        }
        if received.method == Method::GET && received.path == api::ROLES_PATH {
            let roles = self.roles.lock().expect("roles");
            let body = api::Roles {
                roles: roles
                    .iter()
                    .map(|(name, role)| Self::role_json(name, role))
                    .collect(),
                providers: PROVIDERS
                    .iter()
                    .map(|(name, protocol)| api::Provider {
                        name: (*name).to_owned(),
                        protocol: (*protocol).to_owned(),
                    })
                    .collect(),
            };
            return axum::Json(body).into_response();
        }
        let Some(name) = received.path.strip_prefix("/internal/admin/v1/roles/") else {
            return Self::refuse(api::ErrorCode::NotFound, None);
        };
        let mut roles = self.roles.lock().expect("roles");
        let Some(role) = roles.get_mut(name) else {
            return Self::refuse(api::ErrorCode::RoleNotFound, None);
        };
        let mut generation = self.generation.lock().expect("generation");
        if received.method == Method::PUT {
            let Ok(body) = serde_json::from_str::<api::SetModels>(&received.body) else {
                return Self::refuse(api::ErrorCode::InvalidRequest, None);
            };
            if let Err(code) = Self::check_models(&body.models, role.protocol) {
                return Self::refuse(code, None);
            }
            if body.expected_version != role.version {
                return Self::refuse(api::ErrorCode::VersionConflict, Some(role.version));
            }
            *generation += 1;
            role.stored = Some(body.models);
            role.version = *generation;
            role.updated_by = Some((*actor).to_owned());
            return axum::Json(Self::role_json(name, role)).into_response();
        }
        if received.method == Method::DELETE {
            let expected = received
                .query
                .as_deref()
                .and_then(|query| query.strip_prefix("expected_version="))
                .and_then(|version| version.parse::<i64>().ok());
            let Some(expected) = expected else {
                return Self::refuse(api::ErrorCode::InvalidRequest, None);
            };
            if expected != role.version {
                return Self::refuse(api::ErrorCode::VersionConflict, Some(role.version));
            }
            if role.stored.is_some() {
                *generation += 1;
                role.stored = None;
                role.version = *generation;
                role.updated_by = Some((*actor).to_owned());
            }
            return axum::Json(Self::role_json(name, role)).into_response();
        }
        StatusCode::METHOD_NOT_ALLOWED.into_response()
    }
}

/// Decrements the stand-in's in-flight count when a request's answer is done or dropped.
struct InFlight(Arc<StandIn>);

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

async fn stand_in_handler(
    axum::extract::State(stand_in): axum::extract::State<Arc<StandIn>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let received = Received {
        method,
        path: uri.path().to_owned(),
        query: uri.query().map(str::to_owned),
        headers: headers
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_owned(),
                    String::from_utf8_lossy(value.as_bytes()).into_owned(),
                )
            })
            .collect(),
        body: String::from_utf8_lossy(&body).into_owned(),
    };
    stand_in.in_flight.fetch_add(1, Ordering::SeqCst);
    let _in_flight = InFlight(Arc::clone(&stand_in));
    stand_in
        .received
        .lock()
        .expect("received")
        .push(received.clone());
    let mode = stand_in.mode.lock().expect("mode").clone();
    match mode {
        Mode::Emulate => stand_in.emulate(&received),
        Mode::Answer(status, body) => (
            StatusCode::from_u16(status).expect("a status"),
            [("content-type", "application/json")],
            body,
        )
            .into_response(),
        Mode::Hang => std::future::pending().await,
    }
}

async fn start_stand_in() -> (Arc<StandIn>, SocketAddr) {
    let stand_in = StandIn::new();
    let app = Router::new()
        .fallback(stand_in_handler)
        .with_state(Arc::clone(&stand_in));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the stand-in");
    let address = listener.local_addr().expect("the stand-in's address");
    tokio::spawn(async move { axum::serve(listener, app).await });
    (stand_in, address)
}

// ---------------------------------------------------------------------------------------------
// The identity service and its users.
// ---------------------------------------------------------------------------------------------

/// The identity binary on `product` with the event log on, trusting the test issuer's tokens
/// besides its own (for the exchanged tokens no request through ext_authz can carry), and with
/// `admin_url` as `TRUSTIOT_AI_GATEWAY_ADMIN_URL` when given.
fn start(product: &ProductPostgres, issuer: &TestIssuer, admin_url: Option<&str>) -> Service {
    let key = TestKey::new(84);
    let mut published: Value = serde_json::from_str(&issuer.jwks()).expect("the issuer's key set");
    published["keys"]
        .as_array_mut()
        .expect("keys")
        .push(key.jwk(KID));
    let files = KeyFiles::with(
        "ai-model-roles",
        &signing_keys(&[(&key, KID, 1, 0)]),
        &published.to_string(),
    );
    let routes = json!([
        {"host": APP, "path": "/api/identity", "match": "PathPrefix", "service": "identity"},
    ]);
    let mut env = files.env();
    env.extend([
        ("TRUSTIOT_INGRESS_ROUTES".into(), routes.to_string()),
        ("TRUSTIOT__DATABASE__USERNAME".into(), "identity_app".into()),
        ("TRUSTIOT__DATABASE__PASSWORD".into(), TEST_PASSWORD.into()),
        (
            "TRUSTIOT__DATABASE__PLATFORM_USERNAME".into(),
            "identity_platform".into(),
        ),
        (
            "TRUSTIOT__DATABASE__PLATFORM_PASSWORD".into(),
            TEST_PASSWORD.into(),
        ),
    ]);
    if let Some(url) = admin_url {
        env.push(("TRUSTIOT_AI_GATEWAY_ADMIN_URL".into(), url.to_owned()));
    }
    env.extend(
        product
            .platform_env()
            .into_iter()
            .map(|(name, value)| (name.to_owned(), value)),
    );
    let env: Vec<(&str, &str)> = env.iter().map(|(n, v)| (n.as_str(), v.as_str())).collect();
    let service = Service::start(&env);
    service.wait_ready(READY_LIMIT);
    service
}

struct User {
    id: String,
    cookie: String,
}

/// A user (a platform administrator when `platform_admin`), with `role` in `tenant` when given,
/// and an active session.
async fn user(
    admin: &mut PgConnection,
    platform_admin: bool,
    member: Option<(&str, &str)>,
) -> User {
    let id = UserId::generate().to_string();
    sqlx::query(INSERT_USER)
        .bind(&id)
        .bind(platform_admin)
        .execute(&mut *admin)
        .await
        .expect("insert the user");
    if let Some((tenant, role)) = member {
        sqlx::query(INSERT_MEMBERSHIP)
            .bind(tenant)
            .bind(&id)
            .bind(role)
            .execute(&mut *admin)
            .await
            .expect("insert the membership");
    }
    let secret = new_secret();
    sqlx::query(INSERT_SESSION)
        .bind(secret.hash().as_bytes().as_slice())
        .bind(&id)
        .bind(member.map(|(tenant, _)| tenant))
        .execute(&mut *admin)
        .await
        .expect("insert the session");
    User {
        id,
        cookie: cookie(&secret),
    }
}

/// The ext_authz check for the SPA's `method path` request with `cookie` and `extra` headers.
fn check(
    service: &Service,
    method: &str,
    path: &str,
    cookie: &str,
    extra: &[(&str, &str)],
) -> support::Response {
    let mut headers = vec![("Host", APP), ("Cookie", cookie)];
    headers.extend_from_slice(extra);
    request(
        service.address,
        method,
        &format!("/internal/ext-authz{path}"),
        &headers,
    )
    .expect("call the ext_authz check")
}

/// The token the cookie check signs for `cookie`'s requests to the identity API.
fn token(service: &Service, cookie: &str) -> String {
    let response = check(service, "GET", ROLES, cookie, &[]);
    assert_eq!(response.status, 200, "{}", response.raw());
    response.header_values("x-internal-identity")[0].to_owned()
}

/// What a request to the API carries.
struct Call<'a> {
    method: &'a str,
    target: String,
    token: Option<&'a str>,
    content_type: Option<&'a str>,
    extra: Vec<(&'a str, &'a str)>,
    body: String,
}

impl<'a> Call<'a> {
    fn new(method: &'a str, target: impl Into<String>, token: &'a str) -> Self {
        let json = method != "GET";
        Self {
            method,
            target: target.into(),
            token: Some(token),
            content_type: json.then_some("application/json"),
            extra: vec![("Sec-Fetch-Site", "same-origin")],
            body: String::new(),
        }
    }

    fn body(mut self, body: &Value) -> Self {
        self.body = body.to_string();
        self
    }

    fn headers(&self) -> Vec<(&str, &str)> {
        let mut headers = vec![("Host", APP)];
        if let Some(content_type) = self.content_type {
            headers.push(("Content-Type", content_type));
        }
        if let Some(token) = self.token {
            headers.push(("x-internal-identity", token));
        }
        headers.extend(self.extra.iter().copied());
        headers
    }

    fn send(&self, service: &Service) -> support::Response {
        self.send_within(service, Duration::from_secs(20))
    }

    fn send_within(&self, service: &Service, limit: Duration) -> support::Response {
        let response = request_within(
            service.address,
            self.method,
            &self.target,
            &self.headers(),
            self.body.as_bytes(),
            limit,
        )
        .expect("call the model role API");
        // The handlers' answers; a 401 comes from the kit's identity extractor, before them.
        if response.status != 401 {
            assert_eq!(
                response.header_values("cache-control"),
                ["no-store"],
                "{}",
                response.raw()
            );
        }
        response
    }
}

fn role_path(role: &str) -> String {
    format!("{ROLES}/{role}")
}

fn reset_path(role: &str, version: i64) -> String {
    format!("{ROLES}/{role}?expected_version={version}")
}

fn set_body(models: &[&str], version: i64) -> Value {
    json!({"models": models, "expected_version": version})
}

fn assert_error(response: &support::Response, status: u16, code: &str, key: &str, what: &str) {
    assert_eq!(response.status, status, "{what}: {}", response.raw());
    let body = response.json();
    assert_eq!(body["code"], code, "{what}");
    assert_eq!(body["message_key"], key, "{what}");
}

fn assert_models_error(response: &support::Response, key: &str, what: &str) {
    assert_error(
        response,
        422,
        "validation_failed",
        "error.validation_failed",
        what,
    );
    assert_eq!(
        response.json()["field_errors"],
        json!([{"field": "/models", "message_key": key}]),
        "{what}"
    );
}

/// Waits until the stand-in got `count` requests in all.
fn wait_received(stand_in: &StandIn, count: usize, what: &str) {
    let deadline = Instant::now() + RECEIVED_LIMIT;
    while stand_in.count() < count {
        assert!(
            Instant::now() < deadline,
            "the stand-in did not get {what} within {RECEIVED_LIMIT:?}"
        );
        thread::sleep(POLL_EVERY);
    }
}

/// The model role events in the outbox: (tenant, actor, payload).
async fn outbox_events(admin: &mut PgConnection) -> Vec<(Option<String>, Option<String>, Value)> {
    let rows: Vec<(Option<String>, Option<String>, String)> = sqlx::query_as(OUTBOX_EVENTS)
        .fetch_all(admin)
        .await
        .expect("outbox events");
    rows.into_iter()
        .map(|(tenant, actor, payload)| {
            (
                tenant,
                actor,
                serde_json::from_str(&payload).expect("payload JSON"),
            )
        })
        .collect()
}

/// The model role events of the platform's operation log, as the back office reads them, once
/// there are `count`.
fn wait_logged(service: &Service, token: &str, count: usize) -> Vec<Value> {
    let deadline = Instant::now() + EVENT_LIMIT;
    loop {
        let response = request(
            service.address,
            "GET",
            PLATFORM_LOG,
            &[("Host", APP), ("x-internal-identity", token)],
        )
        .expect("read the platform event log");
        assert_eq!(response.status, 200, "{}", response.raw());
        let events: Vec<Value> = response.json()["events"]
            .as_array()
            .expect("events")
            .iter()
            .filter(|event| event["type"] == EVENT_TYPE)
            .cloned()
            .collect();
        if events.len() >= count {
            return events;
        }
        assert!(
            Instant::now() < deadline,
            "the operation log did not show {count} {EVENT_TYPE} within {EVENT_LIMIT:?}: {events:?}"
        );
        thread::sleep(Duration::from_millis(200));
    }
}

fn create_tenant(service: &Service, token: &str, name: &str) -> String {
    let response = request_with_body(
        service.address,
        "POST",
        TENANTS,
        &[
            ("Host", APP),
            ("Content-Type", "application/json"),
            ("x-internal-identity", token),
        ],
        json!({ "name": name }).to_string().as_bytes(),
    )
    .expect("call the tenant API");
    assert_eq!(response.status, 201, "{}", response.body);
    response.json()["tenant_id"]
        .as_str()
        .expect("tenant_id")
        .to_owned()
}

#[tokio::test(flavor = "multi_thread")]
async fn platform_administrators_change_model_roles_through_the_ai_gateway() {
    let Some(product) = product_postgres(&["identity_app", "identity_platform"]).await else {
        return;
    };
    let issuer = TestIssuer::new();
    let (stand_in, gateway) = start_stand_in().await;
    let service = start(&product, &issuer, Some(&format!("http://{gateway}")));
    let mut admin = product.superuser().await;

    let platform = user(&mut admin, true, None).await;
    let platform_token = token(&service, &platform.cookie);
    let tenant = create_tenant(&service, &platform_token, "Plant A");
    let tenant_admin = user(&mut admin, false, Some((&tenant, "admin"))).await;
    let plain = user(&mut admin, false, None).await;
    let actor = format!("user:{}", platform.id);

    // 1. No token, or a token that is not valid: 401 on every route, nothing sent.
    let routes = [
        ("GET", ROLES.to_owned(), Value::Null),
        ("PUT", role_path("main"), set_body(&["anthropic/x"], 0)),
        ("DELETE", reset_path("main", 0), Value::Null),
    ];
    for (method, target, body) in &routes {
        for token in [None, Some("not-a-token")] {
            let mut call = Call::new(method, target.clone(), "");
            call.token = token;
            if !body.is_null() {
                call = call.body(body);
            }
            assert_error(
                &call.send(&service),
                401,
                "unauthenticated",
                "error.unauthenticated",
                &format!("{method} {target} {token:?}"),
            );
        }
    }

    // 2. A tenant administrator, a user without a tenant, and the platform administrator's
    //    token exchanged by another service: 403 on every route, nothing sent.
    let mut exchanged = TestIssuer::claims(ServiceName::Identity, None);
    exchanged.sub = platform.id.parse().expect("a user ID");
    exchanged.platform_admin = true;
    exchanged.act = Some(TokenActor {
        sub: ServiceName::Assistant,
    });
    exchanged.sid = None;
    let exchanged = issuer.token(&exchanged);
    let refused_tokens = [
        (
            "a tenant administrator",
            token(&service, &tenant_admin.cookie),
        ),
        ("a user without a tenant", token(&service, &plain.cookie)),
        ("an exchanged token", exchanged),
    ];
    for (who, token) in &refused_tokens {
        for (method, target, body) in &routes {
            let mut call = Call::new(method, target.clone(), token);
            if !body.is_null() {
                call = call.body(body);
            }
            assert_error(
                &call.send(&service),
                403,
                "permission_denied",
                "error.permission_denied",
                &format!("{who}: {method} {target}"),
            );
        }
    }
    assert_eq!(stand_in.count(), 0, "nothing reached the stand-in");
    evidence(format!(
        "401 without a valid token; 403 for a tenant administrator, a user without a tenant and \
         an exchanged token on GET, PUT and DELETE; the stand-in got {} requests",
        stand_in.count()
    ));

    // 3. The platform administrator reads the roles: the stand-in's, with the actor built from
    //    the token, and none of the request's own headers: not its cookie, its token, its
    //    authorization or its own `ai-gateway-actor`.
    let forged = "user:00000000-0000-7000-8000-000000000bad";
    let mut call = Call::new("GET", ROLES, &platform_token);
    call.extra.extend([
        ("Cookie", platform.cookie.as_str()),
        ("Authorization", "Bearer forged"),
        ("ai-gateway-actor", forged),
    ]);
    let response = call.send(&service);
    assert_eq!(response.status, 200, "{}", response.raw());
    let roles = response.json();
    assert_eq!(
        roles["providers"],
        json!([
            {"name": "anthropic", "protocol": "anthropic-messages"},
            {"name": "openai", "protocol": "openai-decisions"},
        ])
    );
    assert_eq!(
        roles["roles"][0],
        json!({
            "role": "main",
            "protocol": "anthropic-messages",
            "models": ["anthropic/claude-opus-5-5"],
            "configured_models": ["anthropic/claude-opus-5-5"],
            "source": "configuration",
            "version": 0,
            "ignored": null,
            "updated_at": null,
            "updated_by": null
        })
    );
    let read = stand_in.received();
    assert_eq!(read.len(), 1);
    assert_eq!(
        (read[0].method.clone(), read[0].path.as_str()),
        (Method::GET, api::ROLES_PATH)
    );
    assert_forwarded_only_the_actor(&read[0], &actor, &platform, &platform_token, forged);
    evidence(format!(
        "GET 200; the stand-in got {} {} with ai-gateway-actor {:?} (the request also carried a \
         forged one, a cookie, a token and Authorization) and only the headers {:?}",
        read[0].method,
        read[0].path,
        read[0].header(api::ACTOR_HEADER),
        read[0].names()
    ));

    // 4. A model of another protocol: 422 with its key, and the stand-in did not change.
    let before = stand_in.count();
    let mut call = Call::new("PUT", role_path("main"), &platform_token)
        .body(&set_body(&["openai/gpt-6-luna"], 0));
    call.extra.push(("ai-gateway-actor", forged));
    assert_models_error(
        &call.send(&service),
        "ai_model_role.protocol_mismatch",
        "another protocol",
    );
    for (models, key) in [
        (vec!["nobody/model"], "ai_model_role.unknown_provider"),
        (
            vec!["anthropic/a", "anthropic/a"],
            "ai_model_role.invalid_models",
        ),
    ] {
        assert_models_error(
            &Call::new("PUT", role_path("main"), &platform_token)
                .body(&set_body(&models, 0))
                .send(&service),
            key,
            key,
        );
    }
    assert_eq!(stand_in.count(), before + 3);
    let main = stand_in.role("main");
    assert_eq!((main.stored, main.version), (None, 0), "nothing changed");
    evidence(
        "PUT of another protocol's model: 422 /models ai_model_role.protocol_mismatch; an unknown \
         provider and a duplicate: 422 with their keys; the stand-in's role main unchanged \
         (version 0, no stored models)",
    );

    // 5. A change: 200 with the stand-in's answer. The stand-in got the actor from the token,
    //    and a body of exactly the two fields, whatever else the request's body held.
    let before = stand_in.count();
    let mut call = Call::new("PUT", role_path("main"), &platform_token).body(&json!({
        "models": ["anthropic/claude-sonnet-5-5", "anthropic/claude-opus-5-5"],
        "expected_version": 0,
        "role": "router",
        "actor": forged
    }));
    call.extra.extend([
        ("Cookie", platform.cookie.as_str()),
        ("ai-gateway-actor", forged),
    ]);
    let response = call.send(&service);
    assert_eq!(response.status, 200, "{}", response.raw());
    let changed = response.json();
    assert_eq!(changed["models"][0], "anthropic/claude-sonnet-5-5");
    assert_eq!(changed["source"], "database");
    assert_eq!(changed["version"], 1);
    assert_eq!(changed["updated_by"], actor.as_str());
    assert_eq!(changed["updated_at"], "2026-10-29T08:00:00Z");
    let sent = stand_in.since(before);
    assert_eq!(sent.len(), 1, "one write");
    assert_eq!(
        (sent[0].method.clone(), sent[0].path.as_str()),
        (Method::PUT, "/internal/admin/v1/roles/main")
    );
    assert_eq!(
        serde_json::from_str::<Value>(&sent[0].body).expect("JSON"),
        json!({
            "models": ["anthropic/claude-sonnet-5-5", "anthropic/claude-opus-5-5"],
            "expected_version": 0
        })
    );
    assert_eq!(sent[0].header("content-type"), ["application/json"]);
    assert_forwarded_only_the_actor(&sent[0], &actor, &platform, &platform_token, forged);
    evidence(format!(
        "PUT 200 version 1; the stand-in got {} {} body {} with ai-gateway-actor {:?} and only \
         the headers {:?}",
        sent[0].method,
        sent[0].path,
        sent[0].body,
        sent[0].header(api::ACTOR_HEADER),
        sent[0].names()
    ));
    // The event is written before the answer: in the outbox now, platform-wide, with the actor.
    let events = outbox_events(&mut admin).await;
    assert_eq!(
        events,
        [(
            None,
            Some(platform.id.clone()),
            json!({
                "role": "main",
                "action": "set",
                "version": 1,
                "models": ["anthropic/claude-sonnet-5-5", "anthropic/claude-opus-5-5"]
            })
        )]
    );

    // 6. A stale version: 409 with the version now; an unknown role: 404. Nothing changed.
    let response = Call::new("PUT", role_path("main"), &platform_token)
        .body(&set_body(&["anthropic/claude-opus-5-5"], 0))
        .send(&service);
    assert_error(
        &response,
        409,
        "conflict",
        "ai_model_role.version_conflict",
        "stale",
    );
    assert_eq!(response.json()["details"], json!({"current_version": 1}));
    assert_error(
        &Call::new("PUT", role_path("nope"), &platform_token)
            .body(&set_body(&["anthropic/claude-opus-5-5"], 0))
            .send(&service),
        404,
        "not_found",
        "ai_model_role.not_found",
        "unknown role",
    );
    assert_error(
        &Call::new("DELETE", reset_path("main", 0), &platform_token).send(&service),
        409,
        "conflict",
        "ai_model_role.version_conflict",
        "stale reset",
    );
    assert_eq!(stand_in.role("main").version, 1);
    evidence(format!(
        "stale PUT and DELETE: 409 ai_model_role.version_conflict with current_version 1; unknown \
         role: 404 ai_model_role.not_found; outbox: {:?}",
        outbox_events(&mut admin).await
    ));

    // 7. A reset: back to the configured models, a new version and its event. A second reset
    //    changes nothing: same version, no event.
    let response = Call::new("DELETE", reset_path("main", 1), &platform_token).send(&service);
    assert_eq!(response.status, 200, "{}", response.raw());
    assert_eq!(response.json()["source"], "configuration");
    assert_eq!(response.json()["version"], 2);
    let reset = stand_in.received().pop().expect("the reset");
    assert_eq!(
        (
            reset.method.clone(),
            reset.path.as_str(),
            reset.query.as_deref()
        ),
        (
            Method::DELETE,
            "/internal/admin/v1/roles/main",
            Some("expected_version=1")
        )
    );
    assert_forwarded_only_the_actor(&reset, &actor, &platform, &platform_token, forged);
    let response = Call::new("DELETE", reset_path("main", 2), &platform_token).send(&service);
    assert_eq!(response.status, 200, "{}", response.raw());
    assert_eq!(response.json()["version"], 2);
    let events = outbox_events(&mut admin).await;
    assert_eq!(events.len(), 2, "{events:?}");
    assert_eq!(
        events[1],
        (
            None,
            Some(platform.id.clone()),
            json!({
                "role": "main",
                "action": "reset",
                "version": 2,
                "models": ["anthropic/claude-opus-5-5"]
            })
        )
    );
    // The back office sees both in the platform's operation log, with the administrator.
    let logged = wait_logged(&service, &platform_token, 2);
    for event in &logged {
        assert_eq!(event["tenant_id"], Value::Null);
        assert_eq!(
            event["actor"],
            json!({"kind": "user", "user_id": platform.id})
        );
        assert_eq!(event["aggregate_type"], "ai_model_role");
        assert_eq!(event["aggregate_id"], "main");
    }
    evidence(format!(
        "the reset: 200 version 2; a second reset: 200 version 2 without an event; the platform \
         operation log: {}",
        Value::Array(logged)
    ));

    // 8. Refused before anything is sent: cross-site requests (by the cookie check and again by
    //    the handler), changes without a JSON type, bodies the service can tell are wrong, and
    //    names no role can have.
    let before = stand_in.count();
    for (method, target) in [
        ("PUT", role_path("main")),
        ("DELETE", reset_path("main", 2)),
    ] {
        for extra in [
            [("Sec-Fetch-Site", "cross-site")],
            [("Sec-Fetch-Site", "same-site")],
            [("Origin", "https://attacker.example.test")],
        ] {
            let checked = check(&service, method, &target, &platform.cookie, &extra);
            assert_error(
                &checked,
                403,
                "permission_denied",
                "security.cross_site_request",
                &format!("the cookie check: {method} {extra:?}"),
            );
            let mut call = Call::new(method, target.clone(), &platform_token)
                .body(&set_body(&["anthropic/claude-opus-5-5"], 2));
            call.extra = extra.to_vec();
            assert_error(
                &call.send(&service),
                403,
                "permission_denied",
                "security.cross_site_request",
                &format!("the handler: {method} {extra:?}"),
            );
        }
        for content_type in [
            None,
            Some("text/plain"),
            Some("application/x-www-form-urlencoded"),
        ] {
            let mut call = Call::new(method, target.clone(), &platform_token)
                .body(&set_body(&["anthropic/claude-opus-5-5"], 2));
            call.content_type = content_type;
            assert_error(
                &call.send(&service),
                400,
                "invalid_request",
                "error.invalid_request",
                &format!("{method} {content_type:?}"),
            );
        }
    }
    let too_many: Vec<String> = (0..9).map(|n| format!("anthropic/m{n}")).collect();
    let too_many: Vec<&str> = too_many.iter().map(String::as_str).collect();
    for (body, what) in [
        (set_body(&[], 2), "no models"),
        (set_body(&too_many, 2), "nine models"),
    ] {
        assert_models_error(
            &Call::new("PUT", role_path("main"), &platform_token)
                .body(&body)
                .send(&service),
            "ai_model_role.invalid_models",
            what,
        );
    }
    let padded = json!({
        "models": ["anthropic/claude-opus-5-5"],
        "expected_version": 2,
        "padding": "x".repeat(16 << 10)
    });
    for (body, what) in [
        (
            json!({"models": "anthropic/x", "expected_version": 2}),
            "a string",
        ),
        (
            json!({"models": ["anthropic/x"], "expected_version": -1}),
            "a negative version",
        ),
        (
            json!({"models": ["anthropic/x"], "expected_version": 1.5}),
            "a fraction",
        ),
        (json!(["anthropic/x"]), "an array"),
        (padded, "over 16 KiB"),
    ] {
        let response = Call::new("PUT", role_path("main"), &platform_token)
            .body(&body)
            .send(&service);
        assert_error(
            &response,
            400,
            "invalid_request",
            "error.invalid_request",
            what,
        );
    }
    for target in [
        format!("{ROLES}/main"),
        format!("{ROLES}/main?expected_version=-1"),
        format!("{ROLES}/main?expected_version=x"),
    ] {
        assert_error(
            &Call::new("DELETE", target.clone(), &platform_token).send(&service),
            400,
            "invalid_request",
            "error.invalid_request",
            &target,
        );
    }
    for name in ["Main", "a%2Fb", "..", "x".repeat(33).as_str()] {
        assert_error(
            &Call::new("PUT", role_path(name), &platform_token)
                .body(&set_body(&["anthropic/claude-opus-5-5"], 2))
                .send(&service),
            404,
            "not_found",
            "ai_model_role.not_found",
            name,
        );
    }
    assert_eq!(stand_in.count(), before, "nothing was sent");
    evidence(
        "cross-site PUT and DELETE (Sec-Fetch-Site cross-site or same-site, a foreign Origin): \
         403 security.cross_site_request at the ext_authz check and at the handler; no or a \
         non-JSON Content-Type: 400; 0 or 9 models: 422 ai_model_role.invalid_models; a malformed, \
         negative, fractional or over 16 KiB body and a DELETE without a valid expected_version: \
         400; impossible role names: 404; the stand-in got nothing",
    );

    // 9. Answers the service cannot take for a success: a 503 for everything, a change's says
    //    its outcome is unknown, and each change reached the stand-in exactly once.
    let unknown_code = json!({"error": {"code": "something_new", "message": "m"}}).to_string();
    let gateway_unavailable = json!({"error": {"code": "unavailable", "message": "m"}}).to_string();
    for (status, body) in [
        (500, String::new()),
        (504, String::new()),
        (502, "Bad Gateway".to_owned()),
        (503, gateway_unavailable),
        (400, unknown_code.clone()),
        (418, "not json".to_owned()),
        (409, unknown_code),
        (200, "not a role".to_owned()),
    ] {
        stand_in.set_mode(Mode::Answer(status, body.clone()));
        let before = stand_in.count();
        assert_error(
            &Call::new("PUT", role_path("main"), &platform_token)
                .body(&set_body(&["anthropic/claude-opus-5-5"], 2))
                .send(&service),
            503,
            "unavailable",
            "ai_model_role.outcome_unknown",
            &format!("PUT answered {status} {body}"),
        );
        assert_eq!(stand_in.count(), before + 1, "one write for {status}");
        assert_error(
            &Call::new("DELETE", reset_path("main", 2), &platform_token).send(&service),
            503,
            "unavailable",
            "ai_model_role.outcome_unknown",
            &format!("the reset answered {status} {body}"),
        );
        assert_eq!(stand_in.count(), before + 2, "one reset for {status}");
        assert_error(
            &Call::new("GET", ROLES, &platform_token).send(&service),
            503,
            "unavailable",
            "error.unavailable",
            &format!("GET answered {status} {body}"),
        );
    }
    stand_in.set_mode(Mode::Emulate);
    assert_eq!(
        outbox_events(&mut admin).await.len(),
        2,
        "no event without a success"
    );
    evidence(
        "stand-in answers 500, 504, 502, 503 unavailable, 400 and 409 with an unknown code, 418 \
         not JSON and 200 not a role: PUT and DELETE 503 ai_model_role.outcome_unknown, each sent \
         exactly once; GET 503 error.unavailable; no event written",
    );

    // 10. A change whose event cannot be written still answers 200; an ERROR says the
    //     operation log misses it.
    sqlx::query("REVOKE INSERT ON identity.outbox_event FROM identity_platform")
        .execute(&mut admin)
        .await
        .expect("revoke the outbox grant");
    let response = Call::new("PUT", role_path("router"), &platform_token)
        .body(&set_body(&["openai/gpt-6-luna"], 0))
        .send(&service);
    sqlx::query("GRANT INSERT ON identity.outbox_event TO identity_platform")
        .execute(&mut admin)
        .await
        .expect("restore the outbox grant");
    assert_eq!(response.status, 200, "{}", response.raw());
    assert_eq!(response.json()["version"], 3);
    let failed = service
        .stdout
        .wait_for_json(EVENT_LIMIT, |line| {
            line["message"].as_str().is_some_and(|message| {
                message.starts_with("could not write identity.ai_model_role_changed")
            })
        })
        .expect("an ERROR for the event that was not written");
    assert_eq!(failed["level"], "ERROR", "{failed}");
    assert_eq!(failed["fields"]["role"], "router", "{failed}");
    assert_eq!(
        outbox_events(&mut admin).await.len(),
        2,
        "the event was not written"
    );
    evidence(format!(
        "outbox INSERT revoked from identity_platform: PUT 200 version 3, no event, logged {failed}"
    ));

    // 11. While the stand-in hangs: four changes wait, a fifth is refused at once without being
    //     sent, and the ext_authz check every request needs answers as usual. The waiting ones
    //     answer 503 after the client's time limit, each sent once.
    stand_in.set_mode(Mode::Hang);
    let before = stand_in.count();
    let done = Arc::new(AtomicUsize::new(0));
    let hung: Vec<_> = (0..4)
        .map(|_| {
            let address = service.address;
            let token = platform_token.clone();
            let done = Arc::clone(&done);
            thread::spawn(move || {
                let body = set_body(&["anthropic/claude-opus-5-5"], 3).to_string();
                let response = request_within(
                    address,
                    "PUT",
                    &role_path("main"),
                    &[
                        ("Host", APP),
                        ("Content-Type", "application/json"),
                        ("x-internal-identity", &token),
                    ],
                    body.as_bytes(),
                    HUNG_ANSWER_LIMIT,
                )
                .expect("a hung change");
                done.fetch_add(1, Ordering::SeqCst);
                response
            })
        })
        .collect();
    wait_received(&stand_in, before + 4, "the four hung changes");
    let response = Call::new("PUT", role_path("main"), &platform_token)
        .body(&set_body(&["anthropic/claude-opus-5-5"], 3))
        .send(&service);
    assert_error(
        &response,
        503,
        "unavailable",
        "error.unavailable",
        "a fifth change",
    );
    let checked = check(&service, "GET", ROLES, &platform.cookie, &[]);
    assert_eq!(checked.status, 200, "{}", checked.raw());
    let checked = check(&service, "GET", ROLES, &plain.cookie, &[]);
    assert_eq!(checked.status, 200, "{}", checked.raw());
    assert_eq!(
        done.load(Ordering::SeqCst),
        0,
        "the checks answered while the changes still waited"
    );
    assert_eq!(stand_in.count(), before + 4, "the fifth was not sent");
    for handle in hung {
        let response = handle.join().expect("the hung change's thread");
        assert_error(
            &response,
            503,
            "unavailable",
            "ai_model_role.outcome_unknown",
            "a hung change",
        );
    }
    assert_eq!(stand_in.count(), before + 4, "each was sent once");
    evidence(
        "stand-in hanging: 4 PUTs reached it; a 5th got 503 error.unavailable and was not sent; \
         the ext_authz check answered 200 for two users while the 4 waited; then the 4 answered \
         503 ai_model_role.outcome_unknown, each sent once",
    );
    stand_in.set_mode(Mode::Emulate);

    // 12. The logs name the operations and never a cookie, a token or the forged actor.
    let logs = service.stop();
    assert!(logs.contains("changed an AI model role"), "{logs}");
    let platform_secret = platform
        .cookie
        .split_once('=')
        .map(|(_, secret)| secret)
        .expect("a cookie value");
    for (what, value) in [
        ("the session cookie", platform_secret),
        ("the token", platform_token.as_str()),
        ("the forged actor", forged),
        ("the forged authorization", "Bearer forged"),
    ] {
        assert!(!logs.contains(value), "{what} is in the logs");
    }
    evidence(
        "the logs have changed an AI model role and no session cookie, token, forged actor or \
         forged authorization",
    );
    drop(product);
}

/// The stand-in got `actor` in exactly one `ai-gateway-actor`, and nothing of the user's
/// request: no cookie, no token, no authorization, not the forged actor.
fn assert_forwarded_only_the_actor(
    received: &Received,
    actor: &str,
    user: &User,
    token: &str,
    forged: &str,
) {
    assert_eq!(received.header(api::ACTOR_HEADER), [actor], "{received:?}");
    for name in [
        "cookie",
        "authorization",
        "x-internal-identity",
        "sec-fetch-site",
        "origin",
    ] {
        assert!(received.header(name).is_empty(), "{name}: {received:?}");
    }
    let secret = user
        .cookie
        .split_once('=')
        .map(|(_, secret)| secret)
        .expect("a cookie value");
    for (name, value) in &received.headers {
        for leaked in [secret, token, forged] {
            assert!(!value.contains(leaked), "{name} carries {leaked}");
        }
    }
}

/// Without `TRUSTIOT_AI_GATEWAY_ADMIN_URL`, and with an address nothing listens on: 503 on every
/// route for a platform administrator (a WARN or nothing at startup, the service is ready), and
/// 403 for anybody else, as before.
#[tokio::test(flavor = "multi_thread")]
async fn without_a_reachable_admin_api_the_routes_answer_503() {
    let Some(product) = product_postgres(&["identity_app", "identity_platform"]).await else {
        return;
    };
    let issuer = TestIssuer::new();
    // A port that was free a moment ago: nothing listens there.
    let closed = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let closed_url = format!("http://{}", closed.local_addr().expect("address"));
    drop(closed);
    let mut admin = product.superuser().await;
    let platform = user(&mut admin, true, None).await;
    let plain = user(&mut admin, false, None).await;

    for (what, url) in [
        ("not set", None),
        ("unreachable", Some(closed_url.as_str())),
    ] {
        let service = start(&product, &issuer, url);
        let platform_token = token(&service, &platform.cookie);
        let plain_token = token(&service, &plain.cookie);
        for (method, target, body) in [
            ("GET", ROLES.to_owned(), Value::Null),
            (
                "PUT",
                role_path("main"),
                set_body(&["anthropic/claude-opus-5-5"], 0),
            ),
            ("DELETE", reset_path("main", 0), Value::Null),
        ] {
            let mut call = Call::new(method, target.clone(), &platform_token);
            if !body.is_null() {
                call = call.body(&body);
            }
            assert_error(
                &call.send(&service),
                503,
                "unavailable",
                "error.unavailable",
                &format!("{what}: {method} {target}"),
            );
            let mut call = Call::new(method, target.clone(), &plain_token);
            if !body.is_null() {
                call = call.body(&body);
            }
            assert_error(
                &call.send(&service),
                403,
                "permission_denied",
                "error.permission_denied",
                &format!("{what}: {method} {target}"),
            );
        }
        let logs = service.stop();
        let expected = if url.is_none() {
            "TRUSTIOT_AI_GATEWAY_ADMIN_URL is not set"
        } else {
            "a model role request to the AI gateway failed"
        };
        assert!(logs.contains(expected), "{what}: {logs}");
        evidence(format!(
            "admin URL {what}: GET, PUT and DELETE 503 error.unavailable for the platform \
             administrator, 403 for a user without a tenant; logged {expected:?}"
        ));
    }
}
