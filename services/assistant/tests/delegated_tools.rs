//! The first real tools, calling other services as the user through the question's run
//! delegation (Laysi/trustiot-v2#591). Everything runs on loopback: the stand-in AI gateway of
//! `support`, and one stand-in server that is both the identity service's token exchange (it
//! issues real internal identity tokens, signed by a test key) and the telemetry query and assets
//! services (they verify those tokens as the services do, for their own audience, and answer
//! with the services' own response types).
//!
//! Time: every model call stands for 61 seconds passing. The stand-in services accept a token
//! only in the "minute" it was exchanged in, so a tool that reused a token from an earlier step
//! would be refused, as a token past its 60 seconds is; no test waits for a minute to pass.

mod support;

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use assistant::AgentError;
use assistant::callee::Callee;
use assistant::delegation::{QuestionRun, QuestionRuns, RUN_END_MARGIN, StartError};
use assistant::history::{EntryKind, Principal};
use assistant::model::ToolCall;
use assistant::tools::device_models::{self, DeviceModels};
use assistant::tools::telemetry::{self, TelemetryLatest, TelemetrySeries};
use assistant::tools::{Delegation, Tool, ToolContext, ToolRegistry};
use axum::Router;
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use chrono::{TimeDelta, Utc};
use contract::api::identity::{CreateRunDelegation, ExchangeRunDelegation};
use contract::client::{Endpoint, InternalClient};
use contract::{
    Actor, DeviceModelId, ErrorBody, ErrorCode, INTERNAL_IDENTITY_HEADER, InternalClaims,
    ServiceName, TenantGrant, TenantId, TenantRole, service,
};
use serde_json::{Value, json};
use service_kit::identity::UserIdentity;
use support::{Capture, Recorded, Reply, StandIn, WAIT, agent, fixture, limits, ok};
use telemetry_query::history::{Interval, Latest, LatestPoint, Reading, Series, SeriesPoint};
use testkit::identity::TestIssuer;
use tokio::net::TcpListener;
use tokio::time::Instant;
use uuid::Uuid;

const DEVICE: &str = "0192f6e0-7a3c-7d4e-8f00-0123456789ab";
const DELEGATION: &str = "delegation.for.question";

fn unix_now() -> i64 {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("after the epoch");
    i64::try_from(elapsed.as_secs()).expect("seconds fit")
}

/// A call a stand-in service received.
#[derive(Clone, Debug)]
struct Received {
    path: String,
    query: Option<String>,
    token: String,
    /// The token's claims, when it verified for the service's audience.
    claims: Option<InternalClaims>,
    /// The stand-in minute the call came in.
    minute: u64,
    status: u16,
}

#[derive(Default)]
struct Log {
    creates: Vec<Value>,
    /// The expiry of the last delegation issued, Unix seconds.
    expires_at: Option<i64>,
    /// The audience of every exchange the identity service was asked for, in order.
    exchanges: Vec<ServiceName>,
    /// Every token the identity service issued, with the minute it was issued in, in order.
    /// Two exchanges for one audience in the same second give the same bytes (the claims have
    /// no nonce), so a token can appear more than once.
    issued: Vec<(String, u64)>,
    received: Vec<Received>,
}

/// The stand-in identity service and callees, and what they saw.
struct World {
    issuer: TestIssuer,
    /// The user's request, as the ingress signed it for the assistant.
    request: InternalClaims,
    /// The stand-in minute: each model call adds one.
    minute: AtomicU64,
    /// Seconds from creation to the delegation's expiry, as the identity service says.
    expires_in: i64,
    create_refusal: Option<ErrorCode>,
    /// The refusal of the n-th exchange (0-based); later ones repeat the last entry, an empty
    /// list refuses none.
    exchange_refusals: Vec<Option<ErrorCode>>,
    /// The statuses the services answer with, one per call in order; 200 when used up.
    statuses: Mutex<Vec<u16>>,
    /// Readings in each series answered.
    series_len: usize,
    /// Put in every error body the services send: it must reach neither the model nor the logs.
    secret: String,
    log: Mutex<Log>,
}

impl World {
    fn new() -> Self {
        let tenant = TenantGrant {
            id: TenantId::generate(),
            role: TenantRole::Viewer,
        };
        Self {
            issuer: TestIssuer::new(),
            request: TestIssuer::claims(ServiceName::Assistant, Some(tenant)),
            minute: AtomicU64::new(0),
            expires_in: 900,
            create_refusal: None,
            exchange_refusals: Vec::new(),
            statuses: Mutex::new(Vec::new()),
            series_len: 3,
            secret: format!("secret detail {}", Uuid::now_v7()),
            log: Mutex::new(Log::default()),
        }
    }

    fn log(&self) -> std::sync::MutexGuard<'_, Log> {
        self.log.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The user's verified identity at the assistant.
    fn user(&self) -> UserIdentity {
        self.issuer.user(ServiceName::Assistant, &self.request)
    }

    fn principal(&self) -> Principal {
        Principal {
            tenant_id: self.request.tenant.expect("a tenant").id,
            user_id: self.request.sub,
        }
    }

    fn refusal(&self, index: usize) -> Option<ErrorCode> {
        self.exchange_refusals
            .get(index)
            .or_else(|| self.exchange_refusals.last())
            .copied()
            .flatten()
    }
}

/// An M1 error body that carries `detail`.
fn refuse(code: ErrorCode, detail: &str) -> Response {
    let body = ErrorBody::new(code).with_detail("reason", detail);
    (body.http_status(), axum::Json(body)).into_response()
}

fn token_of(headers: &HeaderMap) -> String {
    headers
        .get(INTERNAL_IDENTITY_HEADER)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned()
}

async fn create(State(world): State<Arc<World>>, axum::Json(body): axum::Json<Value>) -> Response {
    world.log().creates.push(body);
    if let Some(code) = world.create_refusal {
        return refuse(code, &world.secret);
    }
    let expires_at = unix_now() + world.expires_in;
    world.log().expires_at = Some(expires_at);
    axum::Json(json!({"delegation": DELEGATION, "expires_at": expires_at})).into_response()
}

async fn exchange(
    State(world): State<Arc<World>>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<Value>,
) -> Response {
    assert_eq!(
        token_of(&headers),
        DELEGATION,
        "the delegation is exchanged"
    );
    let audience: ServiceName =
        serde_json::from_value(body["audience"].clone()).expect("an audience");
    let index = {
        let mut log = world.log();
        log.exchanges.push(audience);
        log.exchanges.len() - 1
    };
    if let Some(code) = world.refusal(index) {
        return refuse(code, &world.secret);
    }
    // As the identity service exchanges: the user, the tenant and role, `act` naming the
    // holder, no session, 60 seconds.
    let iat = unix_now();
    let claims = InternalClaims {
        iss: ServiceName::Identity,
        sub: world.request.sub,
        aud: audience,
        iat,
        exp: iat + 60,
        tenant: world.request.tenant,
        platform_admin: false,
        act: Some(Actor {
            sub: ServiceName::Assistant,
        }),
        sid: None,
    };
    let token = world.issuer.token(&claims);
    world
        .log()
        .issued
        .push((token.clone(), world.minute.load(Ordering::SeqCst)));
    axum::Json(json!({"token": token})).into_response()
}

/// A stand-in service's check of a call: the token verifies for `audience` and is from this
/// minute. Answers the configured status, with the secret in the body, or `None` to go on.
fn admit(
    world: &World,
    audience: ServiceName,
    path: String,
    query: Option<String>,
    headers: &HeaderMap,
) -> Option<Response> {
    let token = token_of(headers);
    let claims = world
        .issuer
        .verifier(audience)
        .verify(&token)
        .ok()
        .map(|user| user.claims().clone());
    let mut log = world.log();
    let minute = world.minute.load(Ordering::SeqCst);
    let current = log
        .issued
        .iter()
        .any(|(issued, at)| *issued == token && *at == minute);
    let status = if claims.is_none() || !current {
        401
    } else {
        let mut statuses = world
            .statuses
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if statuses.is_empty() {
            200
        } else {
            statuses.remove(0)
        }
    };
    log.received.push(Received {
        path,
        query,
        token,
        claims,
        minute,
        status,
    });
    if status == 200 {
        return None;
    }
    let code = match status {
        401 => ErrorCode::Unauthenticated,
        403 => ErrorCode::PermissionDenied,
        404 => ErrorCode::NotFound,
        422 => ErrorCode::ValidationFailed,
        _ => ErrorCode::Unavailable,
    };
    let mut response = refuse(code, &world.secret);
    *response.status_mut() = StatusCode::from_u16(status).expect("a status");
    Some(response)
}

fn device_of(device_id: &str) -> Uuid {
    Uuid::try_parse(device_id).expect("the tool sends a UUID")
}

async fn series(
    State(world): State<Arc<World>>,
    Path(device_id): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let path = format!("/api/telemetry/v1/devices/{device_id}/series");
    if let Some(refused) = admit(&world, ServiceName::TelemetryQuery, path, query, &headers) {
        return refused;
    }
    let now = Utc::now();
    let points = (0..world.series_len)
        .map(|index| SeriesPoint {
            time: now - TimeDelta::hours(i64::try_from(index).expect("small") + 1),
            count: 60,
            min: Some(228.0),
            max: Some(231.0),
            avg: Some(229.5),
            last: Reading::Float(Some(230.25)),
        })
        .collect();
    axum::Json(Series {
        device_id: device_of(&device_id),
        point: "voltage".into(),
        tz: "Asia/Taipei".into(),
        interval: Interval::Hour1,
        from: now - TimeDelta::days(1),
        to: now,
        truncated: false,
        earliest: now - TimeDelta::days(90),
        points,
    })
    .into_response()
}

async fn latest(
    State(world): State<Arc<World>>,
    Path(device_id): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let path = format!("/api/telemetry/v1/devices/{device_id}/latest");
    if let Some(refused) = admit(&world, ServiceName::TelemetryQuery, path, query, &headers) {
        return refused;
    }
    let now = Utc::now();
    axum::Json(Latest {
        device_id: device_of(&device_id),
        earliest: now - TimeDelta::days(7),
        points: vec![
            LatestPoint {
                point: "status".into(),
                time: now,
                value: Reading::String("running".into()),
            },
            LatestPoint {
                point: "voltage".into(),
                time: now,
                value: Reading::Float(Some(229.75)),
            },
        ],
    })
    .into_response()
}

async fn models(
    State(world): State<Arc<World>>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let path = device_models::MODELS_PATH.to_owned();
    if let Some(refused) = admit(&world, ServiceName::Assets, path, query, &headers) {
        return refused;
    }
    let now = Utc::now();
    let summary = |name: &str, points| assets::device_models::DeviceModelSummary {
        model_id: DeviceModelId::generate(),
        name: name.into(),
        revision: 3,
        point_count: points,
        created_at: now,
        updated_at: now,
    };
    axum::Json(assets::device_models::DeviceModelList {
        models: vec![summary("power-meter", 12), summary("chiller", 40)],
    })
    .into_response()
}

/// Serves the stand-in identity exchange and services; returns its origin.
async fn serve(world: Arc<World>) -> String {
    let router = Router::new()
        .route(CreateRunDelegation::ROUTE, post(create))
        .route(ExchangeRunDelegation::ROUTE, post(exchange))
        .route(telemetry_query::history::SERIES_PATH, get(series))
        .route(telemetry_query::history::LATEST_PATH, get(latest))
        .route(assets::device_models::MODELS_PATH, get(models))
        .with_state(world);
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a loopback port");
    let address = listener.local_addr().expect("the local address");
    tokio::spawn(async move {
        axum::serve(listener, router)
            .await
            .expect("serve the stand-in services");
    });
    format!("http://{address}")
}

/// The three tools, calling the stand-in services.
fn tools(origin: &str) -> Vec<Arc<dyn Tool>> {
    let telemetry =
        Callee::new(ServiceName::TelemetryQuery, origin, WAIT).expect("a telemetry client");
    let assets = Callee::new(ServiceName::Assets, origin, WAIT).expect("an assets client");
    vec![
        TelemetrySeries::new(telemetry.clone()),
        TelemetryLatest::new(telemetry),
        DeviceModels::new(assets),
    ]
}

fn runs(origin: &str) -> QuestionRuns {
    let identity = InternalClient::<service::Identity>::new(origin, Duration::from_secs(30))
        .expect("an identity client");
    QuestionRuns::new(identity, Duration::from_secs(900))
}

/// A model step that calls `calls`, made from the recorded step: its thinking, then these tool
/// calls.
fn step(index: usize, calls: &[(&str, Value)]) -> String {
    let mut message: Value =
        serde_json::from_str(&support::tool_step(index)).expect("the fixture is JSON");
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

/// The AI gateway stand-in: answers `steps` in order, then the recorded answer. Each call is a
/// minute later for the stand-in services.
fn model(world: &Arc<World>, steps: Vec<Reply>) -> support::Responder {
    let world = Arc::clone(world);
    let steps = Arc::new(Mutex::new(steps));
    Arc::new(
        move |_request: Recorded, _index: usize| -> support::BoxFuture<Reply> {
            world.minute.fetch_add(1, Ordering::SeqCst);
            let next = {
                let mut steps = steps.lock().unwrap_or_else(PoisonError::into_inner);
                if steps.is_empty() {
                    None
                } else {
                    Some(steps.remove(0))
                }
            };
            Box::pin(async move { next.unwrap_or_else(|| ok(fixture("answer_end_turn.json"))) })
        },
    )
}

/// One tool result: its call, whether it is an error, its text.
#[derive(Debug)]
struct Result_ {
    is_error: bool,
    text: String,
}

/// The tool results of the last message of `request`.
fn results(request: &Recorded) -> Vec<Result_> {
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
        .map(|block| Result_ {
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

fn day_range() -> (String, String) {
    let to = Utc::now();
    let from = to - TimeDelta::days(1);
    let format = |time: chrono::DateTime<Utc>| time.format("%Y-%m-%dT%H:%M:%SZ").to_string();
    (format(from), format(to))
}

fn series_input() -> Value {
    let (from, to) = day_range();
    json!({
        "device_id": DEVICE,
        "point": "voltage",
        "from": from,
        "to": to,
        "tz": "Asia/Taipei",
    })
}

#[tokio::test]
async fn each_tool_call_runs_as_the_user_with_a_fresh_token_for_its_service_and_no_tenant() {
    let world = Arc::new(World::new());
    let origin = serve(Arc::clone(&world)).await;
    // One tool per step; every step is 61 stand-in seconds after the one before.
    let stand_in = StandIn::start(model(
        &world,
        vec![
            ok(step(
                0,
                &[("telemetry_latest", json!({"device_id": DEVICE}))],
            )),
            ok(step(1, &[("telemetry_series", series_input())])),
            ok(step(2, &[("device_models", json!({}))])),
        ],
    ))
    .await;
    let agent = agent(&stand_in, tools(&origin), support::settings());
    let mut conversation = agent.start_conversation(world.principal(), "");
    let run = runs(&origin)
        .start(&world.user(), conversation.principal())
        .await
        .expect("the question's delegation");
    let run_id = run.id();

    let answer = agent
        .ask(&mut conversation, "How is meter 7 doing?", Arc::new(run))
        .await
        .expect("an answer");
    assert!(!answer.text.is_empty());

    let received = {
        let log = world.log();
        // One delegation for the question, under its run ID, for the configured lifetime.
        let [create] = log.creates.as_slice() else {
            panic!("one delegation: {:?}", log.creates);
        };
        assert_eq!(create["run_id"], json!(run_id.to_string()));
        assert_eq!(create["lifetime_secs"], json!(900));
        // One exchange per call, for the service called.
        assert_eq!(
            log.exchanges,
            [
                ServiceName::TelemetryQuery,
                ServiceName::TelemetryQuery,
                ServiceName::Assets
            ]
        );
        let received = log.received.clone();
        assert_eq!(received.len(), 3, "{received:?}");
        let expected = [
            (
                format!("/api/telemetry/v1/devices/{DEVICE}/latest"),
                ServiceName::TelemetryQuery,
            ),
            (
                format!("/api/telemetry/v1/devices/{DEVICE}/series"),
                ServiceName::TelemetryQuery,
            ),
            (device_models::MODELS_PATH.to_owned(), ServiceName::Assets),
        ];
        let principal = world.principal();
        for (index, (call, (path, audience))) in received.iter().zip(&expected).enumerate() {
            assert_eq!(&call.path, path);
            assert_eq!(call.status, 200, "call {index}");
            // The token verified for the callee's audience: the user in the user's tenant, acted for
            // by the assistant, exchanged in the minute of the call.
            let claims = call.claims.as_ref().expect("the token verified");
            assert_eq!(claims.aud, *audience);
            assert_eq!(claims.sub, principal.user_id);
            assert_eq!(claims.tenant_id(), Some(principal.tenant_id));
            assert_eq!(
                claims.act,
                Some(Actor {
                    sub: ServiceName::Assistant
                })
            );
            // Step `index` is stand-in minute `index + 1`: the call came in then, with a token
            // the identity service issued then.
            let step_minute = u64::try_from(index).expect("small") + 1;
            assert_eq!(call.minute, step_minute, "call {index}");
            assert!(
                log.issued
                    .iter()
                    .any(|(token, at)| *token == call.token && *at == step_minute),
                "call {index} used a token exchanged in its own step"
            );
            // No tenant or user anywhere in the request.
            let query = call.query.clone().unwrap_or_default();
            for id in [
                principal.tenant_id.to_string(),
                principal.user_id.to_string(),
            ] {
                assert!(!query.contains(&id) && !call.path.contains(&id), "{call:?}");
            }
            assert!(!query.to_ascii_lowercase().contains("tenant"), "{query}");
        }
        // One exchange per call, each in the minute of its call: the series call, more than 60
        // seconds after the first, did not reuse the first call's token.
        let minutes: Vec<u64> = log.issued.iter().map(|(_, at)| *at).collect();
        assert_eq!(minutes, [1, 2, 3]);
        assert_eq!(received[0].query, None);
        assert_eq!(received[2].query, None);
        let keys: BTreeSet<String> = url_keys(received[1].query.as_deref().expect("a query"));
        assert_eq!(
            keys,
            ["from", "interval", "point", "to", "tz"]
                .map(str::to_owned)
                .into()
        );
        received
    };

    // The control: the first call's token, sent again now, is refused like an expired one.
    let replay = reqwest::Client::new()
        .get(format!("{origin}{}", received[0].path))
        .header(INTERNAL_IDENTITY_HEADER, &received[0].token)
        .send()
        .await
        .expect("the stand-in answers");
    assert_eq!(replay.status(), StatusCode::UNAUTHORIZED);

    // The model got what the services answered for this user.
    let requests = stand_in.requests();
    assert_eq!(requests.len(), 4);
    let latest = &results(&requests[1])[0];
    assert!(!latest.is_error, "{latest:?}");
    let latest: Value = serde_json::from_str(&latest.text).expect("JSON");
    assert_eq!(latest["device_id"], json!(DEVICE));
    assert_eq!(latest["total"], json!(2));
    assert_eq!(latest["rows"][1][0], json!("voltage"));
    assert_eq!(latest["rows"][1][2], json!(229.75));
    assert_eq!(latest["rows"][0][2], json!("running"));
    let series = &results(&requests[2])[0];
    assert!(!series.is_error, "{series:?}");
    let series: Value = serde_json::from_str(&series.text).expect("JSON");
    assert_eq!(series["interval"], json!("1h"));
    assert_eq!(series["rows"].as_array().map(Vec::len), Some(3));
    assert_eq!(series["rows"][0][1], json!(60));
    assert_eq!(series["rows"][0][4], json!(229.5));
    assert_eq!(series["rows"][0][5], json!(230.25));
    let models = &results(&requests[3])[0];
    assert!(!models.is_error, "{models:?}");
    let models: Value = serde_json::from_str(&models.text).expect("JSON");
    assert_eq!(models["total"], json!(2));
    // Ordered by name.
    assert_eq!(models["rows"][0][1], json!("chiller"));
    assert_eq!(models["rows"][1][1], json!("power-meter"));
    assert_eq!(models["rows"][1][3], json!(12));
}

fn url_keys(query: &str) -> BTreeSet<String> {
    query
        .split('&')
        .filter_map(|pair| pair.split('=').next())
        .map(str::to_owned)
        .collect()
}

#[tokio::test]
async fn a_service_refusal_reaches_neither_the_model_nor_the_logs() {
    let mut world = World::new();
    // latest 403 (another tenant's device), series 404, models 422, latest 503, then a raw series
    // over the row limit.
    world.statuses = Mutex::new(vec![403, 404, 422, 503]);
    world.series_len = telemetry::MAX_SERIES_ROWS + 1;
    let world = Arc::new(world);
    let origin = serve(Arc::clone(&world)).await;
    let mut raw = series_input();
    raw["interval"] = json!("raw");
    let stand_in = StandIn::start(model(
        &world,
        vec![
            ok(step(
                0,
                &[("telemetry_latest", json!({"device_id": DEVICE}))],
            )),
            ok(step(1, &[("telemetry_series", series_input())])),
            ok(step(2, &[("device_models", json!({}))])),
            ok(step(
                3,
                &[("telemetry_latest", json!({"device_id": DEVICE}))],
            )),
            ok(step(4, &[("telemetry_series", raw)])),
        ],
    ))
    .await;
    let agent = agent(&stand_in, tools(&origin), support::settings());
    let mut conversation = agent.start_conversation(world.principal(), "");
    let (capture, _guard) = Capture::install();
    let run = runs(&origin)
        .start(&world.user(), conversation.principal())
        .await
        .expect("the question's delegation");

    agent
        .ask(&mut conversation, "Show me meter 7.", Arc::new(run))
        .await
        .expect("the model answers without the data");

    let requests = stand_in.requests();
    assert_eq!(requests.len(), 6);
    let texts: Vec<Result_> = requests[1..]
        .iter()
        .map(|request| results(request).remove(0))
        .collect();
    for result in &texts {
        assert!(result.is_error, "{result:?}");
        assert!(!result.text.contains(&world.secret), "{result:?}");
    }
    assert!(
        texts[0]
            .text
            .starts_with("The service refused this request")
    );
    assert!(
        texts[1]
            .text
            .starts_with("The service refused this request")
    );
    assert!(
        texts[2]
            .text
            .starts_with("The assets service rejected the request")
    );
    assert!(
        texts[3]
            .text
            .starts_with("The tool failed for a reason outside your control")
    );
    assert!(
        texts[4].text.contains("501 readings") && texts[4].text.contains("interval"),
        "{:?}",
        texts[4]
    );
    for request in &requests {
        assert!(!request.body.contains(&world.secret));
    }

    let events = capture.events();
    let outcomes: Vec<String> = events
        .iter()
        .filter(|event| event.message == "tool call completed")
        .map(|event| event.fields["outcome"].clone())
        .collect();
    assert_eq!(
        outcomes,
        ["denied", "denied", "input_error", "failed", "input_error"]
    );
    let refusals: Vec<(String, String)> = events
        .iter()
        .filter(|event| {
            event.message == "a service refused a tool call"
                || event.message == "a tool call to a service failed"
        })
        .map(|event| {
            (
                event.fields["peer"].clone(),
                event.fields["status_class"].clone(),
            )
        })
        .collect();
    assert_eq!(
        refusals,
        [
            ("telemetry-query".to_owned(), "4xx".to_owned()),
            ("telemetry-query".to_owned(), "4xx".to_owned()),
            ("assets".to_owned(), "4xx".to_owned()),
            ("telemetry-query".to_owned(), "5xx".to_owned()),
        ]
    );
    // Nothing of the answers, the tokens or the model's input reaches the logs of the code that
    // runs in the assistant's process (the stand-ins' own crates aside).
    let log = world.log();
    for event in events.iter().filter(|event| {
        ["assistant", "service_kit", "contract"]
            .iter()
            .any(|target| event.target.starts_with(target))
    }) {
        let text = format!("{event:?}");
        assert!(!text.contains(&world.secret), "{event:?}");
        assert!(!text.contains(DEVICE), "{event:?}");
        assert!(!text.contains(DELEGATION), "{event:?}");
        for (token, _) in &log.issued {
            assert!(!text.contains(token.as_str()), "{event:?}");
        }
    }
}

#[tokio::test]
async fn a_revoked_delegation_ends_the_question_without_a_retry() {
    // 401: the user signed out or the session was revoked; 403: the user left the tenant.
    for code in [ErrorCode::Unauthenticated, ErrorCode::PermissionDenied] {
        let mut world = World::new();
        world.exchange_refusals = vec![None, Some(code)];
        let world = Arc::new(world);
        let origin = serve(Arc::clone(&world)).await;
        let stand_in = StandIn::start(model(
            &world,
            vec![
                ok(step(
                    0,
                    &[("telemetry_latest", json!({"device_id": DEVICE}))],
                )),
                // Two calls in the step: the first refusal cancels the other.
                ok(step(
                    1,
                    &[
                        ("telemetry_latest", json!({"device_id": DEVICE})),
                        ("device_models", json!({})),
                    ],
                )),
                ok(step(2, &[("device_models", json!({}))])),
            ],
        ))
        .await;
        let agent = agent(&stand_in, tools(&origin), support::settings());
        let mut conversation = agent.start_conversation(world.principal(), "");
        let run = runs(&origin)
            .start(&world.user(), conversation.principal())
            .await
            .expect("the question's delegation");
        let run = Arc::new(run);

        let error = agent
            .ask(
                &mut conversation,
                "How is meter 7 doing?",
                Arc::clone(&run) as Arc<dyn Delegation>,
            )
            .await
            .expect_err("the question ends");

        assert_eq!(error, AgentError::AccessRevoked, "{code:?}");
        // The model was not called again, and the refused step is not kept.
        assert_eq!(stand_in.requests().len(), 2, "{code:?}");
        let kinds: Vec<EntryKind> = conversation.entries().map(|(kind, _)| kind).collect();
        assert_eq!(
            kinds,
            [EntryKind::Question, EntryKind::Step, EntryKind::ToolResults],
            "{code:?}"
        );
        // Each call of the step asked once at most, and only the first step reached a service.
        let exchanges = world.log().exchanges.len();
        assert!((2..=3).contains(&exchanges), "{code:?}: {exchanges}");
        assert_eq!(world.log().received.len(), 1, "{code:?}");
        // The run asks for nothing more once it was refused.
        assert!(matches!(
            run.token_for(ServiceName::Assets).await,
            Err(assistant::tools::DelegationError::Revoked)
        ));
        assert_eq!(world.log().exchanges.len(), exchanges, "{code:?}");
    }
}

#[tokio::test]
async fn an_unavailable_identity_service_fails_only_the_call() {
    let mut world = World::new();
    world.exchange_refusals = vec![Some(ErrorCode::Unavailable), None];
    let world = Arc::new(world);
    let origin = serve(Arc::clone(&world)).await;
    let stand_in = StandIn::start(model(
        &world,
        vec![ok(step(
            0,
            &[("telemetry_latest", json!({"device_id": DEVICE}))],
        ))],
    ))
    .await;
    let agent = agent(&stand_in, tools(&origin), support::settings());
    let mut conversation = agent.start_conversation(world.principal(), "");
    let run = runs(&origin)
        .start(&world.user(), conversation.principal())
        .await
        .expect("the question's delegation");

    agent
        .ask(&mut conversation, "How is meter 7 doing?", Arc::new(run))
        .await
        .expect("the question goes on and is answered");

    let requests = stand_in.requests();
    assert_eq!(requests.len(), 2);
    let result = &results(&requests[1])[0];
    assert!(result.is_error);
    assert!(
        result
            .text
            .starts_with("The tool failed for a reason outside your control")
    );
    assert!(world.log().received.is_empty());
}

#[tokio::test]
async fn the_question_ends_before_its_delegation_expires() {
    let mut world = World::new();
    // The identity service gives the delegation a little more than the margin: the question has
    // about two seconds.
    let expires_in = i64::try_from(RUN_END_MARGIN.as_secs()).expect("seconds") + 2;
    world.expires_in = expires_in;
    let world = Arc::new(world);
    let origin = serve(Arc::clone(&world)).await;
    // The second model call never answers: only the deadline ends the question.
    let stand_in = StandIn::start(model(
        &world,
        vec![
            ok(step(
                0,
                &[("telemetry_latest", json!({"device_id": DEVICE}))],
            )),
            Reply::Never,
        ],
    ))
    .await;
    let agent = agent(&stand_in, tools(&origin), support::settings());
    let mut conversation = agent.start_conversation(world.principal(), "");
    let run = runs(&origin)
        .start(&world.user(), conversation.principal())
        .await
        .expect("the question's delegation");
    let expires_at = world.log().expires_at.expect("a delegation");
    let deadline = run.deadline().expect("a deadline");
    assert!(
        deadline <= Instant::now() + Duration::from_secs(2),
        "the deadline is the margin before the expiry"
    );

    let error = tokio::time::timeout(
        WAIT,
        agent.ask(&mut conversation, "How is meter 7 doing?", Arc::new(run)),
    )
    .await
    .expect("the question ended")
    .expect_err("no answer");

    assert_eq!(error, AgentError::RunExpired);
    assert!(Instant::now() >= deadline);
    assert!(
        unix_now() < expires_at,
        "the question ended before the delegation expired"
    );
    // The tool call before the deadline ran as the user; the step and its result are kept.
    assert_eq!(world.log().received.len(), 1);
    assert_eq!(world.log().received[0].status, 200);
    let kinds: Vec<EntryKind> = conversation.entries().map(|(kind, _)| kind).collect();
    assert_eq!(
        kinds,
        [EntryKind::Question, EntryKind::Step, EntryKind::ToolResults]
    );
}

#[tokio::test]
async fn a_question_whose_delegation_has_no_time_left_does_not_start() {
    let mut world = World::new();
    world.expires_in = 10;
    let world = Arc::new(world);
    let origin = serve(Arc::clone(&world)).await;
    let stand_in = StandIn::start(model(&world, Vec::new())).await;
    let agent = agent(&stand_in, tools(&origin), support::settings());
    let mut conversation = agent.start_conversation(world.principal(), "");
    let run = runs(&origin)
        .start(&world.user(), conversation.principal())
        .await
        .expect("the question's delegation");

    let error = agent
        .ask(&mut conversation, "How is meter 7 doing?", Arc::new(run))
        .await
        .expect_err("no time left");

    assert_eq!(error, AgentError::RunExpired);
    assert!(stand_in.requests().is_empty());
    assert!(conversation.is_empty());
}

#[tokio::test]
async fn a_question_starts_only_for_the_conversations_user_while_the_session_is_valid() {
    let world = Arc::new(World::new());
    let origin = serve(Arc::clone(&world)).await;
    let runs = runs(&origin);

    // Another user, or the same user in another tenant.
    let mut other_user = world.principal();
    other_user.user_id = contract::UserId::generate();
    let mut other_tenant = world.principal();
    other_tenant.tenant_id = TenantId::generate();
    for principal in [other_user, other_tenant] {
        let error = runs
            .start(&world.user(), principal)
            .await
            .expect_err("not the conversation's user");
        assert!(matches!(error, StartError::NotTheConversationsUser));
        assert_eq!(error.code(), ErrorCode::PermissionDenied);
    }
    assert!(world.log().creates.is_empty());

    // The identity service refuses: the session ended.
    let mut refusing = World::new();
    refusing.create_refusal = Some(ErrorCode::Unauthenticated);
    let refusing = Arc::new(refusing);
    let origin = serve(Arc::clone(&refusing)).await;
    let error = self::runs(&origin)
        .start(&refusing.user(), refusing.principal())
        .await
        .expect_err("refused");
    assert_eq!(error.code(), ErrorCode::Unauthenticated);
    assert!(!format!("{error:?}").contains(&refusing.secret));
}

#[tokio::test]
async fn the_tools_take_no_tenant_or_user_and_refuse_extra_input() {
    let world = Arc::new(World::new());
    let origin = serve(Arc::clone(&world)).await;
    let tools = tools(&origin);
    for tool in &tools {
        let spec = tool.spec();
        assert_eq!(spec.input_schema["additionalProperties"], json!(false));
        let properties = spec.input_schema["properties"]
            .as_object()
            .expect("properties");
        for name in properties.keys() {
            assert!(
                !name.contains("tenant") && !name.contains("user") && !name.contains("org"),
                "{} has {name}",
                spec.name
            );
        }
    }
    let registry = ToolRegistry::new(tools, limits()).expect("the tool set is valid");
    let run: QuestionRun = runs(&origin)
        .start(&world.user(), world.principal())
        .await
        .expect("the question's delegation");
    let context = ToolContext {
        principal: world.principal(),
        conversation_id: Uuid::now_v7(),
        run_id: run.id(),
        delegation: Arc::new(run),
    };
    let other_tenant = TenantId::generate().to_string();
    let call = |id: &str, name: &str, input: Value| ToolCall {
        id: id.into(),
        name: name.into(),
        input,
    };
    let mut series = series_input();
    series["tenant_id"] = json!(other_tenant);
    let results = registry
        .run(
            &context,
            &[
                call(
                    "1",
                    "telemetry_latest",
                    json!({"device_id": DEVICE, "tenant_id": other_tenant}),
                ),
                call("2", "telemetry_series", series),
                call("3", "device_models", json!({"tenant": other_tenant})),
            ],
        )
        .await
        .expect("no access revoked");
    for result in &results {
        assert!(result.output.is_error, "{result:?}");
    }
    assert!(world.log().exchanges.is_empty());
    assert!(world.log().received.is_empty());
}

#[test]
fn the_tools_follow_the_services_routes_and_limits() {
    assert_eq!(
        telemetry::SERIES_PATH,
        telemetry_query::history::SERIES_PATH
    );
    assert_eq!(
        telemetry::LATEST_PATH,
        telemetry_query::history::LATEST_PATH
    );
    assert_eq!(telemetry::RAW_DAYS, telemetry_query::history::RAW_DAYS);
    assert_eq!(
        telemetry::AGGREGATED_MONTHS,
        telemetry_query::history::AGGREGATED_MONTHS
    );
    assert_eq!(
        telemetry::MAX_POINT_LEN,
        telemetry_query::history::MAX_POINT_LEN
    );
    assert!(telemetry::MAX_SERIES_ROWS <= usize::from(telemetry_query::history::MAX_BUCKETS));
    assert_eq!(
        device_models::MODELS_PATH,
        assets::device_models::MODELS_PATH
    );
}
