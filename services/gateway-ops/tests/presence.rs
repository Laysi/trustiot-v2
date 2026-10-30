//! The presence task over a real broker and the product's Postgres (Laysi/trustiot-v2#112): a
//! dropped connection is offline by its last will, a half-open one by the broker's keepalive
//! check, a gateway whose heartbeats stop by the heartbeat timeout, an older session's late last
//! will is ignored, and the API lists the gateways with their presence, last report and
//! configuration versions, each tenant its own.
//!
//! The stand-in gateways are bare MQTT 5 connections with a last will (`RawGateway`), so a test
//! can drop the socket or leave it silent. The agent's keepalive is 60 s, so the broker ends a
//! half-open connection after 90 s; the half-open test sets a keepalive of a few seconds and
//! checks the same rule against it, so nothing here waits for a fixed time it did not configure
//! (CLAUDE.md, "Test time limits"). Each elapsed time is printed for the test report.
//!
//! Starts a throwaway Postgres with TLS and a Mosquitto (podman or docker); skipped without
//! either unless `CI` is set.

mod support;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use contract::{ServiceName, TenantId, TenantRole};
use gateway_ops::presence::rules::Signal;
use gateway_ops::presence::store::{Binding, PresenceStore};
use gateway_ops::presence::{Counts, Parts, Presence, PresenceHandle, Settings};
use protocol::health::{BufferStats, Heartbeat, Online, Status, SystemStats};
use protocol::topic::Channel;
use protocol::{AgentVersion, ConfigVersion, GatewayId, SessionId, UnixMillis, V1};
use serde_json::Value;
use service_kit::identity::ServiceIdentity;
use service_kit::tenant::TenantDb;
use sqlx::postgres::PgPoolOptions;
use support::broker::{Broker, RawGateway};
use support::{Assets, LOGINS, consumer, deliver, hardware_id, platform, wait_for, work};
use testkit::identity::TestIssuer;
use testkit::product::{DATABASE, ProductPostgres, product_postgres};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// How long the presence task may take to connect, or a row to reach the state a test awaits,
/// on a loaded host. Generous: it bounds waits whose expected time the tests check separately.
const LIMIT: Duration = Duration::from_secs(150);

/// The sweep interval the tests configure: short, so a timeout is found soon after it is due.
const SWEEP: Duration = Duration::from_secs(1);

/// The agent's keepalive (decision 6): the broker ends a silent connection after 1.5 times it.
const AGENT_KEEP_ALIVE: u16 = 60;

/// One test at a time: every presence task subscribes to every gateway's status, so tests in
/// parallel would only add load to each other's waits.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// A presence task running until [`Running::stop`].
struct Running {
    handle: PresenceHandle,
    stop: CancellationToken,
    task: JoinHandle<()>,
}

impl Running {
    /// Starts a presence task against `product` and `broker` with the heartbeat grace `grace`,
    /// as `gateway_ops::start` does, and waits until its subscriptions are granted.
    async fn start(product: &ProductPostgres, broker: &Broker, grace: Duration) -> Self {
        let presence = Presence::start(Parts {
            mqtt: broker.service_config(),
            store: PresenceStore::new(&platform(product), work(product)),
            reads: Arc::new(tokio::sync::Semaphore::new(
                gateway_ops::delivery::CONCURRENCY,
            )),
            settings: Settings {
                heartbeat_grace: grace,
                sweep_interval: SWEEP,
            },
        })
        .expect("start the presence task");
        let handle = presence.handle();
        let stop = CancellationToken::new();
        let task = tokio::spawn(presence.run(stop.clone()));
        let running = Self { handle, stop, task };
        wait_for("the presence task subscribed", LIMIT, || {
            let ready = running.handle.check().is_ok();
            async move { ready.then_some(()) }
        })
        .await;
        running
    }

    /// Waits until the counts satisfy `wanted`.
    async fn wait_counts(&self, what: &str, wanted: impl Fn(&Counts) -> bool) -> Counts {
        wait_for(what, LIMIT, || {
            let counts = self.handle.counts();
            let ready = wanted(&counts);
            async move { ready.then_some(counts) }
        })
        .await
    }

    async fn stop(self) {
        self.stop.cancel();
        self.task.await.expect("the presence task ends");
    }
}

/// A gateway's presence row as the superuser reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Row {
    tenant: String,
    state: String,
    reason: Option<String>,
    session: String,
    agent_version: Option<String>,
    interval_s: Option<i32>,
    config_version: Option<i64>,
}

type RowTuple = (
    String,
    String,
    Option<String>,
    String,
    Option<String>,
    Option<i32>,
    Option<i64>,
);

async fn row(product: &ProductPostgres, gateway: &GatewayId) -> Option<Row> {
    let mut admin = product.superuser().await;
    let found: Option<RowTuple> = sqlx::query_as(
        "SELECT tenant_id::text, state, offline_reason, session, agent_version,
            heartbeat_interval_s, config_version
        FROM gateway_ops.gateway_presence WHERE hardware_id = $1",
    )
    .bind(gateway.as_str())
    .fetch_optional(&mut admin)
    .await
    .expect("read the presence row");
    found.map(
        |(tenant, state, reason, session, agent_version, interval_s, config_version)| Row {
            tenant,
            state,
            reason,
            session,
            agent_version,
            interval_s,
            config_version,
        },
    )
}

/// Waits until `gateway`'s row satisfies `wanted`; returns it.
async fn wait_row(
    product: &ProductPostgres,
    what: &str,
    gateway: &GatewayId,
    wanted: impl Fn(&Row) -> bool,
) -> Row {
    wait_for(what, LIMIT, || async {
        row(product, gateway).await.filter(|found| wanted(found))
    })
    .await
}

fn session(name: &str) -> SessionId {
    SessionId::new(format!("{name}-{}", support::broker::random_hex())).expect("a session ID")
}

fn online(session: &SessionId) -> Vec<u8> {
    Status::Online(Online {
        v: V1,
        session: session.clone(),
        agent_version: AgentVersion::new("1.4.0").expect("a version"),
        protocol_rev: protocol::PROTOCOL_REVISION,
    })
    .encode()
    .expect("encode online")
}

fn heartbeat(session: &SessionId, interval_s: u32, config: Option<u64>) -> Vec<u8> {
    let now = UnixMillis::from_system_time(std::time::SystemTime::now()).expect("a time");
    Heartbeat {
        v: V1,
        session: session.clone(),
        sent_at: now,
        interval_s,
        agent_version: AgentVersion::new("1.4.0").expect("a version"),
        config_version: config.and_then(ConfigVersion::new),
        system: SystemStats {
            cpu_percent: 5.0,
            memory_total_bytes: 1 << 30,
            memory_available_bytes: 1 << 29,
            data_total_bytes: 1 << 34,
            data_available_bytes: 1 << 33,
            soc_temp_celsius: None,
            throttling: None,
            booted_at: now,
            uptime_s: 60,
            boot_count: 1,
            agent_uptime_s: 60,
            agent_starts_since_boot: 1,
        },
        buffer: BufferStats {
            pending_records: 0,
            pending_batches: 0,
            oldest_pending: None,
            disk_bytes: 0,
            dropped_records: 0,
        },
        clock_synced: true,
        cert_not_after: now,
        devices: Vec::new(),
    }
    .encode()
    .expect("encode the heartbeat")
}

/// Connects `gateway` with `session` and publishes its retained `online`, as the agent does.
async fn connect(
    broker: &Broker,
    gateway: &GatewayId,
    session: &SessionId,
    keep_alive: u16,
) -> RawGateway {
    let mut raw = RawGateway::connect(broker, gateway, session, keep_alive).await;
    raw.publish(&Channel::Status.topic(gateway), online(session), true, true)
        .await;
    raw
}

/// Binds `gateway` to `tenant` through the projection.
async fn bind(product: &ProductPostgres, tenant: TenantId, gateway: &GatewayId) {
    let assets = Assets::new(product);
    let (consumer, _) = consumer(product).await;
    let (_, bound) = assets.bind(tenant, gateway).await;
    deliver(product, &consumer, &[bound]).await;
}

#[tokio::test]
async fn last_wills_end_dropped_and_half_open_connections_and_older_sessions_are_ignored() {
    let _serial = SERIAL.lock().await;
    let Some(product) = product_postgres(LOGINS).await else {
        return;
    };
    let Some(broker) = Broker::start() else {
        return;
    };
    // A grace far beyond every wait here: only the last wills can make these gateways offline.
    let grace = Duration::from_secs(600);
    let running = Running::start(&product, &broker, grace).await;
    let tenant = TenantId::generate();

    // A dropped connection: the broker publishes the will at once, with no keepalive involved
    // (the agent's 60 s, so 90 s before the broker would notice silence).
    let dropped = hardware_id("go-drop");
    bind(&product, tenant, &dropped).await;
    let first = session("drop");
    let raw = connect(&broker, &dropped, &first, AGENT_KEEP_ALIVE).await;
    let found = wait_row(&product, "the gateway online", &dropped, |row| {
        row.state == "online"
    })
    .await;
    assert_eq!(found.session, first.as_str());
    assert_eq!(found.tenant, tenant.to_string());
    assert_eq!(found.agent_version.as_deref(), Some("1.4.0"));
    let started = Instant::now();
    raw.drop_connection();
    let found = wait_row(&product, "the dropped gateway offline", &dropped, |row| {
        row.state == "offline"
    })
    .await;
    let elapsed = started.elapsed();
    println!("evidence: dropped connection offline after {elapsed:?}");
    assert_eq!(found.reason.as_deref(), Some("connection_lost"));
    assert!(
        elapsed < Duration::from_secs(u64::from(AGENT_KEEP_ALIVE) * 3 / 2),
        "the last will, not the keepalive check, ended the session: {elapsed:?}"
    );

    // A half-open connection: silent, but never closed. The broker publishes the will once
    // 1.5 times the keepalive passed without a packet.
    let keep_alive: u16 = 2;
    let silent = hardware_id("go-silent");
    bind(&product, tenant, &silent).await;
    let quiet = session("silent");
    let mut raw = RawGateway::connect(&broker, &silent, &quiet, keep_alive).await;
    // The broker's clock for the keepalive starts at its last packet from the gateway, this
    // publish; the time is taken before it is sent.
    let started = Instant::now();
    raw.publish(&Channel::Status.topic(&silent), online(&quiet), true, true)
        .await;
    let found = wait_row(&product, "the half-open gateway offline", &silent, |row| {
        row.state == "offline"
    })
    .await;
    let elapsed = started.elapsed();
    println!("evidence: half-open connection (keepalive {keep_alive} s) offline after {elapsed:?}");
    assert_eq!(found.reason.as_deref(), Some("connection_lost"));
    assert_eq!(found.session, quiet.as_str());
    // Mosquitto counts keepalives in whole seconds: not before one keepalive has passed, and the
    // heartbeat timeout (grace 600 s) cannot have done it.
    assert!(
        elapsed >= Duration::from_secs(u64::from(keep_alive)),
        "offline before the keepalive passed: {elapsed:?}"
    );
    drop(raw);

    // A reconnect whose old connection's will arrives after the new session's online.
    let moved = hardware_id("go-moved");
    bind(&product, tenant, &moved).await;
    let old = session("old");
    let new = session("new");
    let old_raw = connect(&broker, &moved, &old, AGENT_KEEP_ALIVE).await;
    wait_row(&product, "the old session online", &moved, |row| {
        row.session == old.as_str()
    })
    .await;
    let mut new_raw = connect(&broker, &moved, &new, AGENT_KEEP_ALIVE).await;
    wait_row(&product, "the new session online", &moved, |row| {
        row.session == new.as_str() && row.state == "online"
    })
    .await;
    let before = running.handle.counts();
    old_raw.drop_connection();
    running
        .wait_counts("the old session's will handled", |counts| {
            counts.ignored > before.ignored
        })
        .await;
    let found = row(&product, &moved).await.expect("a row");
    assert_eq!(
        (found.state.as_str(), found.session.as_str()),
        ("online", new.as_str()),
        "the old session's last will is ignored"
    );

    // A gateway that is not bound: nothing is written.
    let stranger = hardware_id("go-stranger");
    let before = running.handle.counts();
    let stranger_raw = connect(&broker, &stranger, &session("stranger"), AGENT_KEEP_ALIVE).await;
    running
        .wait_counts("the unbound gateway's status handled", |counts| {
            counts.unbound > before.unbound
        })
        .await;
    assert_eq!(row(&product, &stranger).await, None);

    // Garbage on a status topic is refused.
    let before = running.handle.counts();
    new_raw
        .publish(
            &Channel::Status.topic(&moved),
            vec![0xff, 0x00],
            false,
            true,
        )
        .await;
    running
        .wait_counts("the garbage refused", |counts| {
            counts.refused > before.refused
        })
        .await;
    assert_eq!(running.handle.counts().failures, 0);
    drop(stranger_raw);
    running.stop().await;
}

/// Serves the API in process with a new test issuer's keys.
async fn serve_api(product: &ProductPostgres, issuer: &TestIssuer) -> SocketAddr {
    let (db, _) = TenantDb::connect_lazy_with_background(
        product.postgres.options("gateway_ops_app", DATABASE),
        PgPoolOptions::new().max_connections(2),
        ServiceIdentity::new(ServiceName::GatewayOps),
    )
    .expect("the gateway-ops service role's pool");
    let router = gateway_ops::gateways::routes(issuer.verifier(ServiceName::GatewayOps), db, 4);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the API");
    let address = listener.local_addr().expect("the API's address");
    tokio::spawn(async move {
        axum::serve(listener, router).await.expect("serve the API");
    });
    address
}

/// The tenant's gateway list as a viewer of `tenant` reads it.
async fn list(address: SocketAddr, issuer: &TestIssuer, tenant: TenantId) -> Vec<Value> {
    let token = issuer.tenant_token(ServiceName::GatewayOps, tenant, TenantRole::Viewer);
    let response = reqwest::Client::new()
        .get(format!("http://{address}/api/gateway-ops/v1/gateways"))
        .header(contract::INTERNAL_IDENTITY_HEADER, token)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .expect("call the API");
    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(
        response
            .headers()
            .get("cache-control")
            .and_then(|value| value.to_str().ok()),
        Some("no-store")
    );
    let text = response.text().await.expect("read the API's response");
    let body: Value = serde_json::from_str(&text).expect("a JSON body");
    body["gateways"]
        .as_array()
        .expect("a gateway array")
        .clone()
}

#[tokio::test]
async fn stopped_heartbeats_time_out_and_the_api_lists_presence_and_versions() {
    let _serial = SERIAL.lock().await;
    let Some(product) = product_postgres(LOGINS).await else {
        return;
    };
    let Some(broker) = Broker::start() else {
        return;
    };
    let grace = Duration::from_secs(1);
    let running = Running::start(&product, &broker, grace).await;
    let issuer = TestIssuer::new();
    let api = serve_api(&product, &issuer).await;
    let tenant = TenantId::generate();
    let other = TenantId::generate();
    let gateway = hardware_id("go-beat");
    let idle = hardware_id("go-idle");
    bind(&product, tenant, &gateway).await;
    bind(&product, tenant, &idle).await;

    // Before the gateway is heard from, it is listed as unknown with the binding's version.
    let listed = list(api, &issuer, tenant).await;
    assert_eq!(listed.len(), 2, "{listed:?}");
    let entry = |listed: &[Value], gateway: &GatewayId| -> Value {
        listed
            .iter()
            .find(|entry| entry["hardware_id"] == gateway.as_str())
            .cloned()
            .unwrap_or_else(|| panic!("{gateway} listed: {listed:?}"))
    };
    let unknown = entry(&listed, &gateway);
    assert_eq!(unknown["presence"], "unknown", "{unknown}");
    assert_eq!(unknown["last_seen_at"], Value::Null);
    assert_eq!(unknown["applied_config_version"], Value::Null);
    let desired = unknown["desired_config_version"].clone();
    assert!(
        desired.as_u64().is_some(),
        "the binding's version: {unknown}"
    );

    // Heartbeats announcing one second, and the configuration version in use.
    let beating = session("beat");
    let mut raw = connect(&broker, &gateway, &beating, AGENT_KEEP_ALIVE).await;
    let interval_s = 1;
    let started = Instant::now();
    raw.publish(
        &Channel::Heartbeat.topic(&gateway),
        heartbeat(&beating, interval_s, desired.as_u64()),
        false,
        false,
    )
    .await;
    let found = wait_row(&product, "the heartbeat recorded", &gateway, |row| {
        row.interval_s.is_some()
    })
    .await;
    assert_eq!(found.config_version, desired.as_i64());
    let listed = list(api, &issuer, tenant).await;
    let shown = entry(&listed, &gateway);
    assert_eq!(shown["presence"], "online", "{shown}");
    assert_eq!(shown["applied_config_version"], desired, "{shown}");
    assert_eq!(shown["desired_config_version"], desired, "{shown}");
    assert_eq!(shown["agent_version"], "1.4.0");
    assert!(shown["last_seen_at"].is_string(), "{shown}");
    assert!(shown["presence_changed_at"].is_string(), "{shown}");
    assert_eq!(entry(&listed, &idle)["presence"], "unknown");
    // Another tenant sees none of it.
    assert_eq!(list(api, &issuer, other).await, Vec::<Value>::new());

    // No more heartbeats: offline after two intervals and the grace, though the connection is
    // still up (its keepalive is 60 s, so its will cannot come within this test's waits).
    let found = wait_row(&product, "the heartbeat timeout", &gateway, |row| {
        row.state == "offline"
    })
    .await;
    let elapsed = started.elapsed();
    let timeout = Duration::from_secs(u64::from(interval_s) * 2) + grace;
    println!("evidence: heartbeat timeout ({timeout:?} configured) offline after {elapsed:?}");
    assert_eq!(found.reason.as_deref(), Some("heartbeat_timeout"));
    assert!(
        elapsed >= timeout,
        "offline before the timeout: {elapsed:?}"
    );
    let listed = list(api, &issuer, tenant).await;
    let shown = entry(&listed, &gateway);
    assert_eq!(
        (&shown["presence"], &shown["offline_reason"]),
        (&Value::from("offline"), &Value::from("heartbeat_timeout")),
        "{shown}"
    );

    // A late heartbeat of the same session brings it back; its last will then ends it.
    raw.publish(
        &Channel::Heartbeat.topic(&gateway),
        heartbeat(&beating, 60, desired.as_u64()),
        false,
        false,
    )
    .await;
    wait_row(&product, "the gateway back online", &gateway, |row| {
        row.state == "online"
    })
    .await;
    raw.drop_connection();
    let found = wait_row(&product, "the will after the timeout", &gateway, |row| {
        row.state == "offline"
    })
    .await;
    assert_eq!(found.reason.as_deref(), Some("connection_lost"));

    // A new connection starts without the old one's heartbeat: no applied version until it
    // sends one.
    let next = session("next");
    let _next_raw = connect(&broker, &gateway, &next, AGENT_KEEP_ALIVE).await;
    let found = wait_row(&product, "the new session online", &gateway, |row| {
        row.session == next.as_str() && row.state == "online"
    })
    .await;
    assert_eq!((found.interval_s, found.config_version), (None, None));
    let shown = entry(&list(api, &issuer, tenant).await, &gateway);
    assert_eq!(shown["applied_config_version"], Value::Null, "{shown}");
    assert_eq!(shown["desired_config_version"], desired, "{shown}");

    let counts = running.handle.counts();
    assert!(counts.timeouts >= 1, "{counts:?}");
    assert_eq!(counts.failures, 0, "{counts:?}");
    running.stop().await;
}

/// How many heartbeats the flooding gateway sends at once: far more than its queue holds.
const FLOOD: u64 = 2000;

#[tokio::test]
async fn a_flood_delays_no_other_gateway_and_stale_messages_write_no_row() {
    let _serial = SERIAL.lock().await;
    let Some(product) = product_postgres(LOGINS).await else {
        return;
    };
    let Some(broker) = Broker::start() else {
        return;
    };
    let tenant = TenantId::generate();

    // A retained `online` already on the broker when the task subscribes may be from an earlier
    // binding: it creates no row; the gateway's next live message does.
    let replayed = hardware_id("go-replay");
    bind(&product, tenant, &replayed).await;
    let earlier = session("earlier");
    let mut replayed_raw = connect(&broker, &replayed, &earlier, AGENT_KEEP_ALIVE).await;
    let grace = Duration::from_secs(600);
    let running = Running::start(&product, &broker, grace).await;
    running
        .wait_counts("the replayed status handled", |counts| counts.ignored >= 1)
        .await;
    assert_eq!(row(&product, &replayed).await, None);
    replayed_raw
        .publish(
            &Channel::Heartbeat.topic(&replayed),
            heartbeat(&earlier, 60, None),
            false,
            false,
        )
        .await;
    wait_row(&product, "the live heartbeat's row", &replayed, |row| {
        row.state == "online"
    })
    .await;

    // A gateway floods its heartbeats; another's connection drops right after. The other's last
    // will is applied in its own turn, long before the flood would have been worked through.
    let flooding = hardware_id("go-flood");
    let victim = hardware_id("go-victim");
    bind(&product, tenant, &flooding).await;
    bind(&product, tenant, &victim).await;
    let flood_session = session("flood");
    let mut flood_raw = connect(&broker, &flooding, &flood_session, AGENT_KEEP_ALIVE).await;
    let victim_session = session("victim");
    let victim_raw = connect(&broker, &victim, &victim_session, AGENT_KEEP_ALIVE).await;
    wait_row(&product, "the victim online", &victim, |row| {
        row.state == "online"
    })
    .await;
    wait_row(&product, "the flooding gateway online", &flooding, |row| {
        row.state == "online"
    })
    .await;
    let before = running.handle.counts();
    let topic = Channel::Heartbeat.topic(&flooding);
    for _ in 0..FLOOD {
        flood_raw
            .publish(&topic, heartbeat(&flood_session, 60, None), false, false)
            .await;
    }
    let started = Instant::now();
    victim_raw.drop_connection();
    let found = wait_row(&product, "the victim offline", &victim, |row| {
        row.state == "offline"
    })
    .await;
    let elapsed = started.elapsed();
    let counts = running.handle.counts();
    let applied = (counts.written + counts.ignored) - (before.written + before.ignored);
    println!(
        "evidence: {FLOOD} heartbeats from one gateway, then another's connection dropped: \
         offline after {elapsed:?}, with {applied} messages applied and {} dropped by then",
        counts.dropped - before.dropped
    );
    assert_eq!(found.reason.as_deref(), Some("connection_lost"));
    assert!(
        applied < FLOOD,
        "the victim's will waited behind the flood: {applied} messages applied first"
    );
    assert_eq!(counts.failures, 0, "{counts:?}");

    // A binding that ended between the read of the gateway's binding and the write: nothing is
    // written in the former tenant.
    let ended = hardware_id("go-ended");
    bind(&product, tenant, &ended).await;
    let mut admin = product.superuser().await;
    let binding: String = sqlx::query_scalar(
        "UPDATE gateway_ops.gateway_bindings SET unbound_at = now()
        WHERE hardware_id = $1 RETURNING binding_id::text",
    )
    .bind(ended.as_str())
    .fetch_one(&mut admin)
    .await
    .expect("end the binding");
    let store = PresenceStore::new(&platform(&product), work(&product));
    let signal = Signal::Online {
        session: session("ended"),
        agent_version: AgentVersion::new("1.4.0").expect("a version"),
    };
    let stale = Binding {
        tenant,
        binding: binding.parse().expect("a binding ID"),
        bound: true,
    };
    let applied = store
        .apply(&stale, &ended, &signal, false)
        .await
        .expect("the write's transaction");
    assert_eq!(applied, None, "an ended binding is not written");
    assert_eq!(row(&product, &ended).await, None);

    drop(flood_raw);
    running.stop().await;
}
