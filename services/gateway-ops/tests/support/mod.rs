//! Helpers for the service's integration tests, on the product's Postgres schema applied by the
//! migration Job (`testkit::product`): the assets service's events written as the assets
//! service writes them, the handler run by a consumer in the test's thread, the stored
//! configurations, captured log events, and the gateway-ops binary as a real process.
#![allow(
    dead_code,
    reason = "each test binary includes this module and uses a different subset of it"
)]

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read};
use std::net::SocketAddr;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use contract::events::assets::{
    DEVICE_AGGREGATE, DEVICE_MODEL_AGGREGATE, DeviceCreated, DeviceDeleted, DeviceModelCreated,
    DeviceModelDeleted, DeviceUpdated, GATEWAY_BINDING_AGGREGATE, GatewayBound,
};
use contract::{DeviceId, DeviceModelId, Event, EventId, GatewayBindingId, ServiceName, TenantId};
use gateway_ops::projection::{AssetsEventsHandler, SOURCE_SCHEMA, SUBSCRIPTION};
use outbox::{Aggregate, Consumer, ConsumerConfig};
use protocol::config::{
    ByteOrder, DataPoint, DataType, DesiredConfig, Device, DeviceModel, FunctionCode, Host,
    Transport,
};
use protocol::{GatewayId, ModelName, PointKey};
use serde_json::Value;
use service_kit::identity::ServiceIdentity;
use service_kit::tenant::{BackgroundTenantDb, PlatformDb, TenantDb};
use sqlx::PgConnection;
use sqlx::postgres::PgPoolOptions;
use testkit::postgres::TEST_PASSWORD;
use testkit::product::{DATABASE, ProductPostgres};

/// The gateway-ops binary, built by Cargo for these tests.
pub const GATEWAY_OPS: &str = env!("CARGO_BIN_EXE_gateway-ops");

/// The roles these tests log in with: the gateway operations service's two, and the assets
/// service's, which writes the events.
pub const LOGINS: &[&str] = &["assets_app", "gateway_ops_app", "gateway_ops_platform"];

/// The longest a started service may take to listen or to become ready. Generous: test hosts
/// are often loaded (CLAUDE.md, "Test time limits").
pub const START_LIMIT: Duration = Duration::from_secs(60);

/// The longest the consumer may take to apply the events committed so far. A poll reads what is
/// committed at once; the bound is for a loaded host, and for the other tests of the binary,
/// whose consumers share the subscription's lock.
pub const EVENT_LIMIT: Duration = Duration::from_secs(60);

/// Every stored configuration of a gateway, oldest first, across tenants (the superuser).
const VERSIONS: &str = "SELECT tenant_id::text, binding_id::text, version, bound, device_count,
    payload, event_id::text
FROM gateway_ops.desired_configs WHERE hardware_id = $1 ORDER BY version";

/// Whether the subscription processed an event. `$1` subscription, `$2` event ID.
const INBOX: &str = "SELECT count(*) FROM gateway_ops.outbox_inbox
WHERE subscription = $1 AND event_id = $2::uuid";

/// A subscription's dead letter of an event. `$1` subscription, `$2` event ID.
const DEAD_LETTER: &str = "SELECT reason FROM gateway_ops.outbox_dead_letter
WHERE subscription = $1 AND event_id = $2::uuid";

/// A hardware ID no other test uses.
pub fn hardware_id(prefix: &str) -> GatewayId {
    GatewayId::new(format!(
        "{prefix}-{}",
        GatewayBindingId::generate().to_string().replace('-', "")
    ))
    .expect("a hardware ID")
}

/// A model with one data point.
pub fn model(name: &str) -> DeviceModel {
    DeviceModel {
        name: ModelName::new(name).expect("a model name"),
        points: vec![DataPoint {
            key: PointKey::new("voltage").expect("a point key"),
            function_code: FunctionCode::ReadHoldingRegisters,
            address: 0,
            data_type: DataType::U16,
            byte_order: ByteOrder::default(),
            length: None,
            scale: Some(0.1),
            offset: None,
        }],
    }
}

/// A device of `model` behind an RTU-over-TCP converter at a documentation address.
pub fn device(id: DeviceId, model: &DeviceModel, unit_id: u8) -> Device {
    Device {
        id: protocol::DeviceId::new(id.to_string()).expect("a device ID"),
        model: model.name.clone(),
        transport: Transport::RtuOverTcp,
        host: Host::new("192.0.2.10").expect("a host"),
        port: 8899,
        unit_id,
        poll_interval_ms: 60_000,
    }
}

/// The assets service's role, writing events as the assets service does: in the tenant's
/// transaction, in the entity's aggregate.
#[derive(Clone)]
pub struct Assets(BackgroundTenantDb);

impl Assets {
    pub fn new(product: &ProductPostgres) -> Self {
        let db = TenantDb::connect_lazy_with_background(
            product.postgres.options("assets_app", DATABASE),
            PgPoolOptions::new().max_connections(2),
            ServiceIdentity::new(ServiceName::Assets),
        )
        .expect("the assets service role's pool")
        .1;
        Self(db)
    }

    /// Writes `event` in `tenant`'s transaction, in the aggregate `kind`/`id`.
    pub async fn publish<E: Event>(
        &self,
        tenant: TenantId,
        kind: &str,
        id: &str,
        event: &E,
    ) -> EventId {
        let mut tx = self
            .0
            .begin(tenant, "test-assets")
            .await
            .expect("a tenant transaction of the assets service");
        let aggregate = Aggregate::new(kind, id).expect("an aggregate");
        let published = outbox::publish(tx.transaction(), &aggregate, event, Some(tenant))
            .await
            .expect("write the event");
        tx.commit().await.expect("commit the event");
        published.event_id
    }

    /// Binds `gateway` to `tenant`: `assets.gateway_bound`.
    pub async fn bind(&self, tenant: TenantId, gateway: &GatewayId) -> (GatewayBindingId, EventId) {
        let binding_id = GatewayBindingId::generate();
        (binding_id, self.bind_as(tenant, gateway, binding_id).await)
    }

    /// Binds `gateway` to `tenant` with the binding ID `binding_id`: `assets.gateway_bound`.
    pub async fn bind_as(
        &self,
        tenant: TenantId,
        gateway: &GatewayId,
        binding_id: GatewayBindingId,
    ) -> EventId {
        let event = GatewayBound {
            binding_id,
            hardware_id: gateway.clone(),
        };
        let id = binding_id.to_string();
        self.publish(tenant, GATEWAY_BINDING_AGGREGATE, &id, &event)
            .await
    }

    /// Creates `model` in `tenant`: `assets.device_model_created`, revision 1.
    pub async fn create_model(
        &self,
        tenant: TenantId,
        model: &DeviceModel,
    ) -> (DeviceModelId, EventId) {
        let model_id = DeviceModelId::generate();
        (
            model_id,
            self.create_model_as(tenant, model_id, model).await,
        )
    }

    /// Creates `model` with the ID `model_id` in `tenant`: `assets.device_model_created`,
    /// revision 1.
    pub async fn create_model_as(
        &self,
        tenant: TenantId,
        model_id: DeviceModelId,
        model: &DeviceModel,
    ) -> EventId {
        let event = DeviceModelCreated {
            model_id,
            revision: 1,
            model: model.clone(),
        };
        self.publish(
            tenant,
            DEVICE_MODEL_AGGREGATE,
            &model_id.to_string(),
            &event,
        )
        .await
    }

    /// Deletes a model whose last revision was `revision`: `assets.device_model_deleted`.
    pub async fn delete_model(
        &self,
        tenant: TenantId,
        model_id: DeviceModelId,
        model: &DeviceModel,
        revision: u64,
    ) -> EventId {
        let event = DeviceModelDeleted {
            model_id,
            name: model.name.clone(),
            revision,
        };
        self.publish(
            tenant,
            DEVICE_MODEL_AGGREGATE,
            &model_id.to_string(),
            &event,
        )
        .await
    }

    /// `assets.device_created` of `device` (revision 1) on `gateway`.
    pub async fn create_device(
        &self,
        tenant: TenantId,
        gateway: &GatewayId,
        model_id: DeviceModelId,
        binding_id: Option<GatewayBindingId>,
        device_id: DeviceId,
        device: &Device,
    ) -> EventId {
        let event = DeviceCreated {
            device_id,
            revision: 1,
            name: ModelName::new(format!("device-{}", device.unit_id)).expect("a device name"),
            hardware_id: gateway.clone(),
            model_id,
            binding_id,
            device: device.clone(),
        };
        self.publish(tenant, DEVICE_AGGREGATE, &device_id.to_string(), &event)
            .await
    }

    /// `assets.device_updated` of `device` at `revision`.
    #[allow(
        clippy::too_many_arguments,
        reason = "the fields of the event the assets service writes, spelled out by each test"
    )]
    pub async fn update_device(
        &self,
        tenant: TenantId,
        gateway: &GatewayId,
        model_id: DeviceModelId,
        binding_id: Option<GatewayBindingId>,
        device_id: DeviceId,
        revision: u64,
        device: &Device,
    ) -> EventId {
        let event = DeviceUpdated {
            device_id,
            revision,
            name: ModelName::new(format!("device-{}", device.unit_id)).expect("a device name"),
            hardware_id: gateway.clone(),
            model_id,
            binding_id,
            device: device.clone(),
        };
        self.publish(tenant, DEVICE_AGGREGATE, &device_id.to_string(), &event)
            .await
    }

    /// `assets.device_deleted` of a device whose last revision was `revision`.
    pub async fn delete_device(
        &self,
        tenant: TenantId,
        gateway: &GatewayId,
        model_id: DeviceModelId,
        binding_id: Option<GatewayBindingId>,
        device_id: DeviceId,
        revision: u64,
    ) -> EventId {
        let event = DeviceDeleted {
            device_id,
            name: ModelName::new("deleted-device").expect("a device name"),
            hardware_id: gateway.clone(),
            model_id,
            binding_id,
            revision,
        };
        self.publish(tenant, DEVICE_AGGREGATE, &device_id.to_string(), &event)
            .await
    }
}

/// The gateway operations service's role: tenant transactions and the consumer's bookkeeping.
pub fn work(product: &ProductPostgres) -> BackgroundTenantDb {
    TenantDb::connect_lazy_with_background(
        product.postgres.options("gateway_ops_app", DATABASE),
        PgPoolOptions::new().max_connections(3),
        ServiceIdentity::new(ServiceName::GatewayOps),
    )
    .expect("the gateway-ops service role's pool")
    .1
}

/// The gateway operations service's platform role.
pub fn platform(product: &ProductPostgres) -> PlatformDb {
    PlatformDb::connect_lazy(
        product.postgres.options("gateway_ops_platform", DATABASE),
        PgPoolOptions::new().max_connections(3),
    )
    .expect("the gateway-ops platform pool")
}

/// A consumer of the service's subscription with the service's handler, run in the test's
/// thread. The tests of a binary share the subscription, as replicas do: its lock lets one of
/// them deliver at a time, and each event reaches the handler once.
pub async fn consumer(
    product: &ProductPostgres,
) -> (Consumer<AssetsEventsHandler>, AssetsEventsHandler) {
    let platform = platform(product);
    let handler = AssetsEventsHandler::new(&platform);
    let consumer = Consumer::new(
        &platform,
        work(product),
        ConsumerConfig::new(SUBSCRIPTION, SOURCE_SCHEMA).expect("the consumer's settings"),
        handler.clone(),
    );
    consumer
        .ensure_subscription()
        .await
        .expect("create the subscription");
    (consumer, handler)
}

/// Delivers what is readable until the subscription has processed every event of `events`.
pub async fn deliver(
    product: &ProductPostgres,
    consumer: &Consumer<AssetsEventsHandler>,
    events: &[EventId],
) {
    let what = format!("the subscription processing {} event(s)", events.len());
    wait_for(&what, EVENT_LIMIT, || async {
        consumer.poll_once().await.expect("poll the assets log");
        let mut admin = product.superuser().await;
        for event in events {
            if !processed(&mut admin, *event).await {
                return None;
            }
        }
        Some(())
    })
    .await;
}

/// Whether the service's subscription processed `event` (applied it, or dead-lettered it).
pub async fn processed(admin: &mut PgConnection, event: EventId) -> bool {
    let count: i64 = sqlx::query_scalar(INBOX)
        .bind(SUBSCRIPTION)
        .bind(event.to_string())
        .fetch_one(&mut *admin)
        .await
        .expect("read the inbox");
    count == 1 || dead_letter(admin, event).await.is_some()
}

/// The subscription's dead letter of `event`.
pub async fn dead_letter(admin: &mut PgConnection, event: EventId) -> Option<String> {
    sqlx::query_scalar(DEAD_LETTER)
        .bind(SUBSCRIPTION)
        .bind(event.to_string())
        .fetch_optional(admin)
        .await
        .expect("read the dead letters")
}

/// An event's position in the assets service's log.
const LOGGED: &str = "SELECT tx_order, id FROM assets.outbox_event WHERE event_id = $1::uuid";

/// `event`'s position in the assets service's log.
pub async fn position(admin: &mut PgConnection, event: EventId) -> outbox::Position {
    let (tx, id): (i64, i64) = sqlx::query_as(LOGGED)
        .bind(event.to_string())
        .fetch_one(admin)
        .await
        .expect("read the event in the assets log");
    outbox::Position::new(tx, id)
}

/// Moves the service's subscription back to just before `from` and forgets what it processed
/// since (`outbox::Replay::Reprocess`), so every later event reaches the handler again, as when
/// an operator rebuilds the projection.
pub async fn reprocess_from(product: &ProductPostgres, from: outbox::Position) {
    let mut tx = work(product)
        .begin_without_tenant("test-replay")
        .await
        .expect("a transaction of the gateway-ops service role");
    outbox::replay_from(
        &mut tx,
        SUBSCRIPTION,
        from.just_before(),
        outbox::Replay::Reprocess,
    )
    .await
    .expect("replay the subscription");
    tx.commit().await.expect("commit the replay");
}

/// A row of [`VERSIONS`]: tenant, binding, version, bound, device count, payload, event.
type VersionRow = (String, String, i64, bool, i32, Vec<u8>, String);

/// A stored configuration as the superuser reads it, its payload decoded.
#[derive(Clone, Debug)]
pub struct Stored {
    pub tenant_id: TenantId,
    pub binding_id: GatewayBindingId,
    pub version: i64,
    pub bound: bool,
    pub device_count: i32,
    pub config: DesiredConfig,
    pub event_id: EventId,
}

impl Stored {
    /// The device IDs the configuration lists.
    pub fn devices(&self) -> Vec<String> {
        self.config
            .devices
            .iter()
            .map(|device| device.id.as_str().to_owned())
            .collect()
    }
}

/// Every stored configuration of `gateway`, oldest first; each payload is decoded with
/// crates/protocol's codec, which validates it as the gateway does.
pub async fn versions(admin: &mut PgConnection, gateway: &GatewayId) -> Vec<Stored> {
    let rows: Vec<VersionRow> = sqlx::query_as(VERSIONS)
        .bind(gateway.as_str())
        .fetch_all(admin)
        .await
        .expect("read the stored configurations");
    rows.into_iter()
        .map(
            |(tenant, binding, version, bound, device_count, payload, event)| {
                let config = DesiredConfig::decode(&payload)
                    .unwrap_or_else(|error| panic!("version {version} is not valid: {error}"));
                assert_eq!(
                    i64::try_from(config.version.get()).expect("a version"),
                    version,
                    "the payload carries its own version"
                );
                Stored {
                    tenant_id: tenant.parse().expect("a tenant"),
                    binding_id: binding.parse().expect("a binding"),
                    version,
                    bound,
                    device_count,
                    config,
                    event_id: event.parse().expect("an event ID"),
                }
            },
        )
        .collect()
}

/// Polls `check` every 100 ms until it returns `Some`, for at most `limit`; fails naming `what`
/// was awaited.
pub async fn wait_for<T, F, Fut>(what: &str, limit: Duration, mut check: F) -> T
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Option<T>>,
{
    let deadline = Instant::now() + limit;
    loop {
        if let Some(found) = check().await {
            return found;
        }
        assert!(Instant::now() < deadline, "{what}: not within {limit:?}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Each captured event's message and other fields.
type Captured = Vec<(String, BTreeMap<String, String>)>;

/// Captures the log events of the current thread (services/certs/tests/bindings.rs).
#[derive(Clone, Default)]
pub struct Capture(Arc<Mutex<Captured>>);

impl Capture {
    /// Captures until the guard is dropped. tracing caches per callsite whether any subscriber
    /// wants its events; a second dispatcher kept alive for the whole binary makes it ask every
    /// live dispatcher, so a callsite another test's thread reached first is not cached as
    /// unwanted (services/assistant/tests/support/mod.rs, Laysi/trustiot-v2#581).
    pub fn install() -> (Self, tracing::subscriber::DefaultGuard) {
        use tracing_subscriber::layer::SubscriberExt as _;
        static KEEPER: OnceLock<tracing::Dispatch> = OnceLock::new();
        KEEPER.get_or_init(|| tracing::Dispatch::new(tracing::subscriber::NoSubscriber::new()));
        let capture = Self::default();
        let guard =
            tracing::subscriber::set_default(tracing_subscriber::registry().with(capture.clone()));
        (capture, guard)
    }

    /// The fields (and `level`) of every event whose message is `message`.
    pub fn messages(&self, message: &str) -> Vec<BTreeMap<String, String>> {
        self.0
            .lock()
            .expect("the lock")
            .iter()
            .filter(|(seen, _)| seen == message)
            .map(|(_, fields)| fields.clone())
            .collect()
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
        fields.insert("level".to_owned(), event.metadata().level().to_string());
        let message = fields.remove("message").unwrap_or_default();
        self.0.lock().expect("the lock").push((message, fields));
    }
}

/// The lines a child process printed, shared with the thread that reads them.
#[derive(Clone, Default)]
pub struct Output(Arc<Mutex<Vec<String>>>);

impl Output {
    fn collect(&self, stream: impl Read + Send + 'static) {
        let lines = Arc::clone(&self.0);
        thread::spawn(move || {
            for line in BufReader::new(stream).lines() {
                let Ok(line) = line else { break };
                lines.lock().expect("output lock").push(line);
            }
        });
    }

    /// Every line so far.
    pub fn text(&self) -> String {
        self.0.lock().expect("output lock").join("\n")
    }

    /// The JSON lines whose `message` is `message`.
    pub fn messages(&self, message: &str) -> Vec<Value> {
        self.0
            .lock()
            .expect("output lock")
            .iter()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|value| value["message"] == message)
            .collect()
    }
}

/// The gateway-ops service running as a child process.
pub struct Service {
    child: Child,
    pub stdout: Output,
    pub stderr: Output,
    health: SocketAddr,
}

impl Service {
    /// Starts the service on free local ports against `product`, with the roles the chart gives
    /// it, and waits until it listens. The environment is otherwise empty, so the developer's
    /// own variables cannot leak in.
    pub async fn start(product: &ProductPostgres) -> Self {
        let mut child = Command::new(GATEWAY_OPS)
            .env_clear()
            .env("TRUSTIOT__HTTP__LISTEN_ADDR", "127.0.0.1:0")
            .env("TRUSTIOT__HTTP__HEALTH_LISTEN_ADDR", "127.0.0.1:0")
            .env("TRUSTIOT__SHUTDOWN__DRAIN_DELAY", "100ms")
            .env("TRUSTIOT__SHUTDOWN__DRAIN_TIMEOUT", "5s")
            .env("TRUSTIOT__DATABASE__USERNAME", "gateway_ops_app")
            .env("TRUSTIOT__DATABASE__PASSWORD", TEST_PASSWORD)
            .env(
                "TRUSTIOT__DATABASE__PLATFORM_USERNAME",
                "gateway_ops_platform",
            )
            .env("TRUSTIOT__DATABASE__PLATFORM_PASSWORD", TEST_PASSWORD)
            .envs(product.platform_env())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start the gateway-ops service");
        let (stdout, stderr) = (Output::default(), Output::default());
        stdout.collect(child.stdout.take().expect("piped stdout"));
        stderr.collect(child.stderr.take().expect("piped stderr"));
        let deadline = Instant::now() + START_LIMIT;
        let listening = loop {
            if let Some(listening) = stdout.messages("listening").into_iter().next() {
                break listening;
            }
            assert!(
                Instant::now() < deadline,
                "the gateway-ops service did not log `listening` within {START_LIMIT:?}\n\
                 stdout:\n{}\nstderr:\n{}",
                stdout.text(),
                stderr.text()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        let health = listening["fields"]["health_address"]
            .as_str()
            .and_then(|address| address.parse().ok())
            .unwrap_or_else(|| panic!("no health_address in {listening}"));
        Self {
            child,
            stdout,
            stderr,
            health,
        }
    }

    /// `GET /readyz`: the status and the body.
    pub async fn readiness(&self) -> (u16, String) {
        let response = reqwest::Client::new()
            .get(format!("http://{}/readyz", self.health))
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .expect("call /readyz");
        let status = response.status().as_u16();
        (status, response.text().await.expect("read the body"))
    }

    /// Waits until `/readyz` answers 200.
    pub async fn wait_ready(&self) {
        let deadline = Instant::now() + START_LIMIT;
        loop {
            if self.readiness().await.0 == 200 {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "/readyz of the gateway-ops service did not answer 200 within {START_LIMIT:?}\n\
                 stdout:\n{}",
                self.stdout.text()
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Sends SIGTERM and waits for the exit status, at most `limit`.
    pub async fn terminate(&mut self, limit: Duration) -> std::process::ExitStatus {
        let pid = self.child.id().to_string();
        let status = Command::new("kill")
            .args(["-TERM", &pid])
            .status()
            .expect("send SIGTERM");
        assert!(status.success(), "kill -TERM {pid}");
        let deadline = Instant::now() + limit;
        loop {
            if let Some(status) = self.child.try_wait().expect("wait for the service") {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "the gateway-ops service did not exit within {limit:?} of SIGTERM\nstdout:\n{}",
                self.stdout.text()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}
