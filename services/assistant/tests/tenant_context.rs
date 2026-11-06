//! The tenant context a new conversation starts with (Laysi/trustiot-v2#783; the rules are in
//! docs/architecture/ai-assistant.md, 「租戶脈絡」): the user's settings from the request, and the
//! devices and data points of the user's tenant read from assets as that user.
//!
//! Everything runs on loopback: the product's Postgres (`testkit::product`), the public API in
//! the test process, the identity service's token exchange of `dashboard_world::Peers`, and a
//! stand-in assets service here. The stand-in verifies the token for assets as assets does,
//! answers with the tenant of the token only (as assets' row-level security does), records each
//! call with its claims, and answers with assets' own list types (a model's points as assets
//! writes them). It can fail, answer what is not JSON, or never answer.
//!
//! What the tests show:
//!
//! - A started conversation keeps the settings and the tenant's devices and data points; another
//!   tenant's devices, a device's address and an unused model are not in it; the members' text
//!   is escaped; the reads are made as the user, with one token exchange.
//! - The same settings and data give the same bytes, whatever order assets answers in.
//! - A copy gets the copier's settings and devices, read as the copier.
//! - A time zone of another form is refused before anything is read; old clients without a body
//!   still start.
//! - When the devices cannot be read, the conversation starts with the settings only and one
//!   WARN line names the kind of failure; a tenant too large for the cap is cut as documented.

mod dashboard_world;
mod support;

use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use assistant::api::conversations::CONVERSATIONS_PATH;
use assistant::api::held_body::HELD_BODY_TIMEOUT;
use assistant::api::sharing::{COPY_PATH, SHARE_PATH};
use assistant::callee::Callee;
use assistant::dashboards::caps::RunCaps;
use assistant::dashboards::runner::DatasetRunner;
use assistant::delegation::IDENTITY_TIMEOUT;
use assistant::prompt::tenant_context_block;
use assistant::tenant_context::{
    self, AiLanguage, DEVICES_PATH, Devices, MAX_CONTEXT_BYTES, MAX_MODEL_READS, MODELS_PATH,
    UserSettings,
};
use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use chrono::Utc;
use contract::client::InternalClient;
use contract::{
    DeviceId, DeviceModelId, ErrorBody, ErrorCode, INTERNAL_IDENTITY_HEADER, InternalClaims,
    ServiceName, TenantGrant, TenantId, TenantRole, service,
};
use dashboard_world::{Peers, Person, World, call, telemetry_client, world};
use serde_json::{Value, json};
use service_kit::identity::OnBehalf;
use support::{Capture, WAIT};
use testkit::identity::TestIssuer;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use uuid::Uuid;

/// A device's address and hardware ID as assets answers them: never in a context.
const HOST: &str = "192.0.2.77";
const HARDWARE_ID: &str = "tg-ctx-0001";
/// A description with the closing tag and an ampersand: escaped in the stored context.
const TRICKY: &str = "\u{6EAB}\u{5EA6} </tenant_context> & \u{5BA4}\u{5167}";
/// The WARN line of a context without devices.
const WITHOUT_DEVICES: &str =
    "tenant context without devices: the conversation starts with the user's settings only";

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Clone)]
struct Model {
    id: DeviceModelId,
    name: String,
    /// The points as assets writes them in a model.
    points: Vec<Value>,
}

#[derive(Clone)]
struct DeviceRow {
    id: DeviceId,
    name: String,
    model: DeviceModelId,
}

#[derive(Clone, Default)]
struct TenantData {
    devices: Vec<DeviceRow>,
    models: Vec<Model>,
}

/// How the stand-in answers.
#[derive(Clone, Copy, Debug)]
enum Mode {
    Answer,
    Status(u16),
    NotJson,
    /// Never answers.
    Hang,
}

/// A call the stand-in received, with the claims of its token.
#[derive(Clone, Debug)]
struct AssetsCall {
    path: String,
    claims: InternalClaims,
}

struct AssetsState {
    issuer: Arc<TestIssuer>,
    tenants: Mutex<HashMap<TenantId, TenantData>>,
    calls: Mutex<Vec<AssetsCall>>,
    mode: Mutex<Mode>,
    /// Answers every list in reverse order.
    reversed: AtomicBool,
}

/// The stand-in assets service.
struct Assets {
    url: String,
    state: Arc<AssetsState>,
    task: JoinHandle<()>,
}

impl Drop for Assets {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Assets {
    async fn start(issuer: Arc<TestIssuer>) -> Self {
        let state = Arc::new(AssetsState {
            issuer,
            tenants: Mutex::new(HashMap::new()),
            calls: Mutex::new(Vec::new()),
            mode: Mutex::new(Mode::Answer),
            reversed: AtomicBool::new(false),
        });
        let router = Router::new()
            .route(DEVICES_PATH, get(devices))
            .route(MODELS_PATH, get(models))
            .route(assets::device_models::MODEL_PATH, get(model))
            .with_state(Arc::clone(&state));
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("local address");
        let task = tokio::spawn(async move {
            axum::serve(listener, router)
                .await
                .expect("serve the assets stand-in");
        });
        Self {
            url: format!("http://{address}"),
            state,
            task,
        }
    }

    fn tenant(&self, tenant: TenantId, data: TenantData) {
        lock(&self.state.tenants).insert(tenant, data);
    }

    fn mode(&self, mode: Mode) {
        *lock(&self.state.mode) = mode;
    }

    fn reverse(&self, reversed: bool) {
        self.state.reversed.store(reversed, Ordering::SeqCst);
    }

    fn calls(&self) -> Vec<AssetsCall> {
        lock(&self.state.calls).clone()
    }

    fn paths(&self) -> Vec<String> {
        self.calls().into_iter().map(|call| call.path).collect()
    }
}

fn refuse(code: ErrorCode) -> Response {
    let body = ErrorBody::new(code);
    (body.http_status(), axum::Json(body)).into_response()
}

/// Verifies the call's token for assets and records it; `Ok` with the token's tenant's data, or
/// the answer the mode asks for.
async fn admit(
    state: &AssetsState,
    path: String,
    headers: &HeaderMap,
) -> Result<TenantData, Box<Response>> {
    let token = headers
        .get(INTERNAL_IDENTITY_HEADER)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    let Ok(user) = state.issuer.verifier(ServiceName::Assets).verify(token) else {
        return Err(Box::new(refuse(ErrorCode::Unauthenticated)));
    };
    let claims = user.claims().clone();
    let Some(grant) = claims.tenant else {
        return Err(Box::new(refuse(ErrorCode::PermissionDenied)));
    };
    lock(&state.calls).push(AssetsCall { path, claims });
    let mode = *lock(&state.mode);
    match mode {
        Mode::Answer => {}
        Mode::Status(status) => {
            let status = StatusCode::from_u16(status).expect("a status");
            let body = axum::Json(ErrorBody::new(ErrorCode::Unavailable));
            return Err(Box::new((status, body).into_response()));
        }
        Mode::NotJson => return Err(Box::new((StatusCode::OK, "devices, maybe").into_response())),
        Mode::Hang => std::future::pending::<()>().await,
    }
    let mut data = lock(&state.tenants)
        .get(&grant.id)
        .cloned()
        .unwrap_or_default();
    if state.reversed.load(Ordering::SeqCst) {
        data.devices.reverse();
        data.models.reverse();
        for model in &mut data.models {
            model.points.reverse();
        }
    }
    Ok(data)
}

async fn devices(State(state): State<Arc<AssetsState>>, headers: HeaderMap) -> Response {
    let data = match admit(&state, DEVICES_PATH.to_owned(), &headers).await {
        Ok(data) => data,
        Err(answer) => return *answer,
    };
    let now = Utc::now();
    let devices = data
        .devices
        .iter()
        .map(|device| assets::devices::Device {
            device_id: device.id,
            name: device.name.clone(),
            hardware_id: HARDWARE_ID.into(),
            model_id: device.model,
            transport: "tcp".into(),
            host: HOST.into(),
            port: 502,
            unit_id: 1,
            poll_interval_ms: 60_000,
            revision: 1,
            created_at: now,
            updated_at: now,
        })
        .collect();
    axum::Json(assets::devices::DeviceList { devices }).into_response()
}

async fn models(State(state): State<Arc<AssetsState>>, headers: HeaderMap) -> Response {
    let data = match admit(&state, MODELS_PATH.to_owned(), &headers).await {
        Ok(data) => data,
        Err(answer) => return *answer,
    };
    let now = Utc::now();
    let models = data
        .models
        .iter()
        .map(|model| assets::device_models::DeviceModelSummary {
            model_id: model.id,
            name: model.name.clone(),
            revision: 2,
            point_count: u64::try_from(model.points.len()).expect("fits"),
            created_at: now,
            updated_at: now,
        })
        .collect();
    axum::Json(assets::device_models::DeviceModelList { models }).into_response()
}

async fn model(
    State(state): State<Arc<AssetsState>>,
    Path(model_id): Path<Uuid>,
    headers: HeaderMap,
) -> Response {
    let data = match admit(&state, format!("{MODELS_PATH}/{model_id}"), &headers).await {
        Ok(data) => data,
        Err(answer) => return *answer,
    };
    let Some(model) = data
        .models
        .iter()
        .find(|model| *model.id.as_uuid() == model_id)
    else {
        return refuse(ErrorCode::NotFound);
    };
    let now = Utc::now();
    axum::Json(json!({
        "model_id": model.id,
        "name": model.name,
        "revision": 2,
        "created_at": now,
        "updated_at": now,
        "points": model.points,
    }))
    .into_response()
}

/// A point as assets writes it: register 0 by function code 3, byte order `abcd`.
fn point(key: &str, data_type: &str, scale: Option<f64>, description: Option<&str>) -> Value {
    let mut point = json!({
        "key": key,
        "function_code": 3,
        "address": 0,
        "data_type": data_type,
        "byte_order": "abcd",
    });
    if let Some(scale) = scale {
        point["scale"] = json!(scale);
    }
    if let Some(description) = description {
        point["description"] = json!(description);
    }
    point
}

/// Tenant A's devices: three of two models, and a model no device uses.
struct Plant {
    data: TenantData,
    power: DeviceModelId,
    climate: DeviceModelId,
    boiler: DeviceId,
    annex: DeviceId,
    lab: DeviceId,
}

fn plant() -> Plant {
    let (power, climate, unused) = (
        DeviceModelId::generate(),
        DeviceModelId::generate(),
        DeviceModelId::generate(),
    );
    let (boiler, annex, lab) = (
        DeviceId::generate(),
        DeviceId::generate(),
        DeviceId::generate(),
    );
    let data = TenantData {
        devices: vec![
            DeviceRow {
                id: lab,
                name: "lab-sensor".into(),
                model: climate,
            },
            DeviceRow {
                id: boiler,
                name: "boiler-room".into(),
                model: power,
            },
            DeviceRow {
                id: annex,
                name: "annex-sensor".into(),
                model: climate,
            },
        ],
        models: vec![
            Model {
                id: climate,
                name: "temp-humidity".into(),
                points: vec![
                    point("temperature", "f32", None, Some(TRICKY)),
                    point("humidity", "u16", Some(0.1), Some("Relative humidity, %")),
                ],
            },
            Model {
                id: unused,
                name: "unused-model".into(),
                points: vec![point("unused-point", "u16", None, None)],
            },
            Model {
                id: power,
                name: "power-meter".into(),
                points: vec![
                    point("status", "bool", None, None),
                    point("energy", "u32", None, Some("Energy, kWh")),
                ],
            },
        ],
    };
    Plant {
        data,
        power,
        climate,
        boiler,
        annex,
        lab,
    }
}

/// Another tenant's device, which no context of tenant A may name.
fn other_tenant() -> TenantData {
    let model = DeviceModelId::generate();
    TenantData {
        devices: vec![DeviceRow {
            id: DeviceId::generate(),
            name: "other-tenant-device".into(),
            model,
        }],
        models: vec![Model {
            id: model,
            name: "other-tenant-model".into(),
            points: vec![point("other-tenant-point", "f32", None, None)],
        }],
    }
}

/// A context source reading from `assets` with tokens the stand-in identity of `peers`
/// exchanges.
fn source(peers: &Peers, assets: &Assets) -> tenant_context::Source {
    let identity = InternalClient::<service::Identity>::new(&peers.url, IDENTITY_TIMEOUT)
        .expect("a client for the exchange's stand-in");
    let callee = Callee::new(ServiceName::Assets, &assets.url, WAIT).expect("an assets client");
    tenant_context::Source::new(Some(OnBehalf::new(identity)), Some(callee))
}

/// The public API served in the test process with the context `source`.
struct Api {
    url: String,
    task: JoinHandle<()>,
}

impl Drop for Api {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn api(world: &World, peers: &Peers, source: tenant_context::Source) -> Api {
    let runner = Arc::new(DatasetRunner::new(
        Some(telemetry_client(peers)),
        RunCaps::default(),
    ));
    let routes = world.routes_with_context(peers, runner, true, HELD_BODY_TIMEOUT, source);
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("local address");
    let task = tokio::spawn(async move {
        axum::serve(listener, routes).await.expect("serve the API");
    });
    Api {
        url: format!("http://{address}"),
        task,
    }
}

/// Starts a conversation as `person` with `body`; returns its ID.
async fn start(api: &Api, world: &World, person: &Person, body: &Value) -> String {
    let answer = call(
        &api.url,
        Method::POST,
        CONVERSATIONS_PATH,
        Some(&world.token(person)),
        Some(body),
    )
    .await;
    assert_eq!(answer.status, 201, "{answer:?}");
    answer.json()["id"].as_str().expect("an ID").to_owned()
}

/// The tenant context a conversation keeps, as the model gets it.
async fn stored_context(world: &World, id: &str) -> String {
    let mut admin = world.superuser().await;
    sqlx::query_scalar(
        "SELECT tenant_context FROM assistant.conversations WHERE conversation_id = $1::uuid",
    )
    .bind(id)
    .fetch_one(&mut admin)
    .await
    .expect("the stored preamble")
}

/// The conversations `person` has, deleted or not.
async fn conversations_of(world: &World, person: &Person) -> i64 {
    let mut admin = world.superuser().await;
    sqlx::query_scalar("SELECT count(*) FROM assistant.conversations WHERE user_id = $1::uuid")
        .bind(person.claims.sub.to_string())
        .fetch_one(&mut admin)
        .await
        .expect("count the conversations")
}

fn taipei() -> Value {
    json!({"time_zone": "Asia/Taipei", "ai_language": "zh-TW"})
}

/// Every call was made as `person` in their tenant, on behalf of the assistant.
fn assert_made_as(calls: &[AssetsCall], person: &Person) {
    assert!(!calls.is_empty(), "no call was made");
    for call in calls {
        assert_eq!(call.claims.sub, person.claims.sub, "{call:?}");
        assert_eq!(
            call.claims.tenant.map(|grant| grant.id),
            Some(person.tenant()),
            "{call:?}"
        );
        assert_eq!(
            call.claims.act.as_ref().map(|actor| actor.sub),
            Some(ServiceName::Assistant),
            "{call:?}"
        );
    }
}

/// Acceptance criteria 2 and 3: the stored context has the user's settings and the tenant's
/// devices and data points, read as the user with one token exchange, and the same data gives
/// the same bytes in any order.
#[tokio::test(flavor = "multi_thread")]
async fn a_new_conversation_keeps_the_settings_and_the_tenants_devices() {
    let Some(world) = world().await else { return };
    let peers = Peers::start(Arc::clone(&world.issuer)).await;
    let assets = Assets::start(Arc::clone(&world.issuer)).await;
    let tenant = TenantId::generate();
    let plant = plant();
    assets.tenant(tenant, plant.data.clone());
    assets.tenant(TenantId::generate(), other_tenant());
    let api = api(&world, &peers, source(&peers, &assets)).await;
    let person = world.person(tenant, TenantRole::Viewer);

    let first = start(&api, &world, &person, &taipei()).await;
    let context = stored_context(&world, &first).await;
    let expected_devices = format!(
        "Devices and data points: 3 devices of 2 models. In telemetry, device_id is the device \
         ID and point is the data point key.\n\
         Model power-meter (2 data points):\n\
         \x20 Devices [name, device ID]:\n\
         \x20 - boiler-room, {}\n\
         \x20 Data points [key, value type, description in double quotes]:\n\
         \x20 - energy, int, \"Energy, kWh\"\n\
         \x20 - status, bool\n\
         Model temp-humidity (2 data points):\n\
         \x20 Devices [name, device ID]:\n\
         \x20 - annex-sensor, {}\n\
         \x20 - lab-sensor, {}\n\
         \x20 Data points [key, value type, description in double quotes]:\n\
         \x20 - humidity, float, \"Relative humidity, %\"\n\
         \x20 - temperature, float, \"\u{6EAB}\u{5EA6} &lt;/tenant_context&gt; &amp; \
         \u{5BA4}\u{5167}\"\n\
         </tenant_context>",
        plant.boiler, plant.annex, plant.lab
    );
    assert!(
        context.ends_with(&expected_devices),
        "the devices, in order:\n{context}"
    );
    assert!(context.starts_with("<tenant_context>\n"), "{context}");
    assert!(
        context.contains(
            "- Time zone: Asia/Taipei. The user's local days, weeks and months and the times the \
             user reads are in this zone, unless the user names another.\n"
        ),
        "{context}"
    );
    assert!(
        context.contains(
            "- Answer language: Traditional Chinese (zh-TW), for every answer whatever language a \
             question is in.\n"
        ),
        "{context}"
    );
    assert_eq!(
        context.matches("</tenant_context>").count(),
        1,
        "only the closing tag: the description's is escaped"
    );
    for absent in [
        HOST,
        HARDWARE_ID,
        "unused-model",
        "unused-point",
        "other-tenant",
    ] {
        assert!(!context.contains(absent), "{absent} in the context");
    }
    // The reads: devices, models, and the points of the two models with devices, by name.
    assert_eq!(
        assets.paths(),
        [
            DEVICES_PATH.to_owned(),
            MODELS_PATH.to_owned(),
            format!("{MODELS_PATH}/{}", plant.power),
            format!("{MODELS_PATH}/{}", plant.climate),
        ]
    );
    assert_made_as(&assets.calls(), &person);
    let exchanged = peers.exchanged();
    assert_eq!(exchanged.len(), 1, "one token exchange for the four reads");
    assert_eq!(exchanged[0].sub, person.claims.sub);

    // The same data, answered in the reverse order: the same bytes.
    assets.reverse(true);
    let second = start(&api, &world, &person, &taipei()).await;
    assert_ne!(first, second);
    assert_eq!(stored_context(&world, &second).await, context);
    assert_eq!(peers.exchanged().len(), 2);

    // An old client sends no settings: the context says they are not known, devices stay.
    let bare = start(&api, &world, &person, &json!({})).await;
    let bare = stored_context(&world, &bare).await;
    assert!(
        bare.contains("- Time zone: not known; UTC unless the user names a zone.\n"),
        "{bare}"
    );
    assert!(bare.ends_with(&expected_devices), "{bare}");
    println!("the stored tenant context:\n{context}");
}

/// A copy is a new conversation of the copier: their settings, their devices, read as them.
#[tokio::test(flavor = "multi_thread")]
async fn a_copy_gets_the_copiers_context() {
    let Some(world) = world().await else { return };
    let peers = Peers::start(Arc::clone(&world.issuer)).await;
    let assets = Assets::start(Arc::clone(&world.issuer)).await;
    let tenant = TenantId::generate();
    let plant = plant();
    assets.tenant(tenant, plant.data.clone());
    let api = api(&world, &peers, source(&peers, &assets)).await;
    let owner = world.person(tenant, TenantRole::Editor);
    let reader = world.person(tenant, TenantRole::Viewer);

    let id = start(&api, &world, &owner, &taipei()).await;
    let shared = call(
        &api.url,
        Method::PUT,
        &SHARE_PATH.replace("{conversation_id}", &id),
        Some(&world.token(&owner)),
        None,
    )
    .await;
    assert_eq!(shared.status, 200, "{shared:?}");
    let before = assets.calls().len();
    let copied = call(
        &api.url,
        Method::POST,
        &COPY_PATH.replace("{conversation_id}", &id),
        Some(&world.token(&reader)),
        Some(&json!({"time_zone": "Europe/Berlin", "ai_language": "en"})),
    )
    .await;
    assert_eq!(copied.status, 201, "{copied:?}");
    let copy = copied.json()["id"]
        .as_str()
        .expect("the copy's ID")
        .to_owned();
    let context = stored_context(&world, &copy).await;
    assert!(
        context.contains("- Time zone: Europe/Berlin. "),
        "{context}"
    );
    assert!(
        context.contains(
            "- Answer language: English, for every answer whatever language a question is in.\n"
        ),
        "{context}"
    );
    assert!(
        !context.contains("Asia/Taipei"),
        "not the owner's: {context}"
    );
    assert!(context.contains(&format!("  - lab-sensor, {}\n", plant.lab)));
    let copy_calls = &assets.calls()[before..];
    assert_eq!(copy_calls.len(), 4, "{copy_calls:?}");
    assert_made_as(copy_calls, &reader);
}

/// The settings are checked before anything is read: a time zone of another form is a 422, a
/// language or field the API does not know a 400, and neither stores or reads anything.
#[tokio::test(flavor = "multi_thread")]
async fn settings_of_another_form_are_refused_before_anything_is_read() {
    let Some(world) = world().await else { return };
    let peers = Peers::start(Arc::clone(&world.issuer)).await;
    let assets = Assets::start(Arc::clone(&world.issuer)).await;
    let tenant = TenantId::generate();
    assets.tenant(tenant, plant().data);
    let api = api(&world, &peers, source(&peers, &assets)).await;
    let person = world.person(tenant, TenantRole::Viewer);
    let token = world.token(&person);

    let long = format!("Area/{}", "a".repeat(60));
    for zone in ["", "Asia/<b>", "Asia/Tai pei", "/Asia", long.as_str()] {
        let answer = call(
            &api.url,
            Method::POST,
            CONVERSATIONS_PATH,
            Some(&token),
            Some(&json!({"time_zone": zone})),
        )
        .await;
        assert_eq!(answer.status, 422, "{zone:?}: {answer:?}");
        let body = answer.json();
        assert_eq!(body["code"], "validation_failed", "{body}");
        assert_eq!(body["field_errors"][0]["field"], "/time_zone", "{body}");
        assert_eq!(
            body["field_errors"][0]["message_key"], "conversation.time_zone_invalid",
            "{body}"
        );
        assert_eq!(body["field_errors"][0]["details"]["max"], 64, "{body}");
    }
    for body in [
        json!({"ai_language": "fr"}),
        json!({"time_zone": 8}),
        json!({"time_zone": "Asia/Taipei", "tenant_id": tenant}),
        json!(["Asia/Taipei"]),
    ] {
        let answer = call(
            &api.url,
            Method::POST,
            CONVERSATIONS_PATH,
            Some(&token),
            Some(&body),
        )
        .await;
        assert_eq!(answer.status, 400, "{body}: {answer:?}");
        assert_eq!(
            answer.error(),
            ("invalid_request".into(), "conversation.body_invalid".into()),
            "{body}"
        );
    }
    assert!(assets.calls().is_empty(), "{:?}", assets.paths());
    assert!(peers.exchanged().is_empty());
    assert_eq!(conversations_of(&world, &person).await, 0);
}

/// Acceptance criterion 5 through the API: assets fails, and the conversation starts with the
/// settings only.
#[tokio::test(flavor = "multi_thread")]
async fn a_start_whose_devices_fail_starts_with_the_settings_only() {
    let Some(world) = world().await else { return };
    let peers = Peers::start(Arc::clone(&world.issuer)).await;
    let assets = Assets::start(Arc::clone(&world.issuer)).await;
    let tenant = TenantId::generate();
    assets.tenant(tenant, plant().data);
    assets.mode(Mode::Status(503));
    let api = api(&world, &peers, source(&peers, &assets)).await;
    let person = world.person(tenant, TenantRole::Viewer);

    let id = start(&api, &world, &person, &taipei()).await;
    let settings = UserSettings {
        time_zone: Some("Asia/Taipei".into()),
        ai_language: Some(AiLanguage::ZhTw),
    };
    assert_eq!(
        stored_context(&world, &id).await,
        tenant_context_block(&tenant_context::render(&settings, &Devices::Unavailable))
    );
    assert_eq!(
        assets.paths(),
        [DEVICES_PATH],
        "the first failure ends the reads"
    );
}

/// A start or copy that will be refused makes no call to assets or the token exchange: a copy of
/// a conversation the copier cannot copy is a 404, and a user at the daily limit gets the 429,
/// both before the context is read (the security review of Laysi/trustiot-v2#783: otherwise each
/// refused request costs a token exchange and up to 18 assets reads).
#[tokio::test(flavor = "multi_thread")]
async fn refused_starts_and_copies_read_nothing() {
    let Some(world) = world().await else { return };
    let peers = Peers::start(Arc::clone(&world.issuer)).await;
    let assets = Assets::start(Arc::clone(&world.issuer)).await;
    let tenant = TenantId::generate();
    assets.tenant(tenant, plant().data);
    let api = api(&world, &peers, source(&peers, &assets)).await;
    // The same routes without assets, to reach the limit without reading anything.
    let plain = self::api(&world, &peers, tenant_context::Source::settings_only()).await;
    let owner = world.person(tenant, TenantRole::Editor);
    let reader = world.person(tenant, TenantRole::Viewer);
    let copy_of = |id: &str| COPY_PATH.replace("{conversation_id}", id);

    // Not shared, missing, and the copier's tenant is not the source's: 404, nothing read.
    let private = start(&plain, &world, &owner, &json!({})).await;
    let elsewhere = world.person(TenantId::generate(), TenantRole::Viewer);
    for (who, id) in [
        (&reader, private.clone()),
        (&reader, Uuid::now_v7().to_string()),
        (&elsewhere, private.clone()),
    ] {
        let answer = call(
            &api.url,
            Method::POST,
            &copy_of(&id),
            Some(&world.token(who)),
            Some(&taipei()),
        )
        .await;
        assert_eq!(answer.status, 404, "{answer:?}");
    }
    assert!(assets.calls().is_empty(), "{:?}", assets.paths());
    assert!(peers.exchanged().is_empty());

    // The reader reaches the daily limit without assets, then neither a start nor a copy of a
    // shared conversation reads anything.
    let shared = call(
        &api.url,
        Method::PUT,
        &SHARE_PATH.replace("{conversation_id}", &private),
        Some(&world.token(&owner)),
        None,
    )
    .await;
    assert_eq!(shared.status, 200, "{shared:?}");
    let max = usize::try_from(assistant::store::MAX_STARTS_PER_DAY).expect("small");
    for _ in 0..max {
        start(&plain, &world, &reader, &json!({})).await;
    }
    for path in [CONVERSATIONS_PATH.to_owned(), copy_of(&private)] {
        let answer = call(
            &api.url,
            Method::POST,
            &path,
            Some(&world.token(&reader)),
            Some(&taipei()),
        )
        .await;
        assert_eq!(answer.status, 429, "{path}: {answer:?}");
        assert_eq!(
            answer.error(),
            ("rate_limited".into(), "conversation.start_limited".into())
        );
        assert!(answer.header("retry-after").is_some(), "{answer:?}");
    }
    assert!(assets.calls().is_empty(), "{:?}", assets.paths());
    assert!(peers.exchanged().is_empty());
    assert_eq!(
        conversations_of(&world, &reader).await,
        i64::try_from(max).expect("small")
    );

    // The owner, under the limit, copies their own: read as the owner.
    let copied = call(
        &api.url,
        Method::POST,
        &copy_of(&private),
        Some(&world.token(&owner)),
        Some(&taipei()),
    )
    .await;
    assert_eq!(copied.status, 201, "{copied:?}");
    assert_made_as(&assets.calls(), &owner);
}

/// A person of a new tenant, outside the world (no Postgres).
fn person(issuer: &TestIssuer) -> (Person, service_kit::identity::UserIdentity) {
    let claims = TestIssuer::claims(
        ServiceName::Assistant,
        Some(TenantGrant {
            id: TenantId::generate(),
            role: TenantRole::Viewer,
        }),
    );
    let user = issuer.user(ServiceName::Assistant, &claims);
    (Person { claims }, user)
}

/// Acceptance criterion 5: every way the reads can fail gives the settings-only context and one
/// WARN line that names the kind of failure and nothing of the tenant's data.
#[tokio::test]
async fn every_failure_gives_the_settings_only_and_one_warning() {
    let issuer = Arc::new(TestIssuer::new());
    let peers = Peers::start(Arc::clone(&issuer)).await;
    let assets = Assets::start(Arc::clone(&issuer)).await;
    let (person, user) = person(&issuer);
    assets.tenant(person.tenant(), plant().data);
    let settings = UserSettings {
        time_zone: Some("Europe/Berlin".into()),
        ai_language: None,
    };
    let expected = tenant_context::render(&settings, &Devices::Unavailable);
    // The reads' bound, as the test configures it: a stand-in that never answers reaches it.
    let bound = Duration::from_secs(2);
    let cases: [(&str, Mode, bool, &str); 5] = [
        ("assets answers 503", Mode::Status(503), false, "failed"),
        ("assets answers 404", Mode::Status(404), false, "denied"),
        (
            "assets answers what is not JSON",
            Mode::NotJson,
            false,
            "failed",
        ),
        ("assets never answers", Mode::Hang, false, "timeout"),
        (
            "the token exchange is refused",
            Mode::Answer,
            true,
            "failed",
        ),
    ];
    for (what, mode, refuse_exchange, kind) in cases {
        assets.mode(mode);
        peers.refuse_exchanges(refuse_exchange.then_some(ErrorCode::Unavailable));
        let source = source(&peers, &assets).with_timeout(bound);
        let (capture, guard) = Capture::install();
        let text = source.build(&user, person.principal(), &settings).await;
        drop(guard);
        assert_eq!(text, expected, "{what}");
        // The context's own warnings; the internal client logs a failed exchange too (its own
        // WARN, from `contract::client`).
        let warnings: Vec<_> = capture
            .events()
            .into_iter()
            .filter(|event| {
                event.level == tracing::Level::WARN && event.target == "assistant::tenant_context"
            })
            .collect();
        assert_eq!(warnings.len(), 1, "{what}: {warnings:?}");
        let warning = &warnings[0];
        assert_eq!(warning.message, WITHOUT_DEVICES, "{what}");
        assert_eq!(
            warning
                .fields
                .keys()
                .map(String::as_str)
                .collect::<BTreeSet<_>>(),
            BTreeSet::from(["kind", "tenant_id", "user_id"]),
            "{what}"
        );
        assert_eq!(warning.fields["kind"], kind, "{what}");
        assert_eq!(warning.fields["user_id"], person.claims.sub.to_string());
        assert_eq!(warning.fields["tenant_id"], person.tenant().to_string());
    }
    peers.refuse_exchanges(None);

    // Without the exchange or assets configured: the same text, and no warning.
    let (capture, guard) = Capture::install();
    let text = tenant_context::Source::settings_only()
        .build(&user, person.principal(), &settings)
        .await;
    drop(guard);
    assert_eq!(text, expected);
    assert!(
        capture
            .events()
            .iter()
            .all(|event| event.level != tracing::Level::WARN)
    );
}

/// Acceptance criterion 4 against the service's answers: a tenant over the cap is cut by the
/// documented rule, at most [`MAX_MODEL_READS`] models' points are read, and the cut is the same
/// every time.
#[tokio::test]
async fn a_large_tenant_is_cut_to_the_cap_the_same_way_each_time() {
    let issuer = Arc::new(TestIssuer::new());
    let peers = Peers::start(Arc::clone(&issuer)).await;
    let assets = Assets::start(Arc::clone(&issuer)).await;
    let (person, user) = person(&issuer);
    let description = "\u{4F9B}\u{6C34}\u{6EAB}\u{5EA6}".repeat(30);
    let mut data = TenantData::default();
    for model in 0..20 {
        let id = DeviceModelId::generate();
        data.models.push(Model {
            id,
            name: format!("model-{model:02}"),
            points: (0..30)
                .map(|n| point(&format!("point-{n:02}"), "i32", None, Some(&description)))
                .collect(),
        });
        for n in 0..5 {
            data.devices.push(DeviceRow {
                id: DeviceId::generate(),
                name: format!("device-{model:02}-{n}"),
                model: id,
            });
        }
    }
    assets.tenant(person.tenant(), data);
    let settings = UserSettings {
        time_zone: Some("Asia/Taipei".into()),
        ai_language: Some(AiLanguage::Auto),
    };
    let source = source(&peers, &assets);
    let text = source.build(&user, person.principal(), &settings).await;
    let block = tenant_context_block(&text);
    assert!(block.len() <= MAX_CONTEXT_BYTES, "{}", block.len());
    assert!(
        text.contains("- Time zone: Asia/Taipei."),
        "the settings stay"
    );
    assert!(
        text.contains("Data point descriptions are left out to fit the size limit."),
        "descriptions go first"
    );
    assert!(!text.contains('\u{4F9B}'), "no description is left");
    let note = text.rsplit_once('\n').expect("lines").1;
    assert!(
        note.starts_with("Not listed here: ") && note.ends_with(" and device_models."),
        "{note}"
    );
    assert!(text.contains("Model model-00 (30 data points):\n"));
    let details = assets
        .paths()
        .iter()
        .filter(|path| path.starts_with(&format!("{MODELS_PATH}/")))
        .count();
    assert_eq!(details, MAX_MODEL_READS);

    assets.reverse(true);
    assert_eq!(
        source.build(&user, person.principal(), &settings).await,
        text,
        "the same cut in any order"
    );
}
