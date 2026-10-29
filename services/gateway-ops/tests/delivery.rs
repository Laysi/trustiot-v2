//! The delivery over a real broker and the product's Postgres (Laysi/trustiot-v2#720): retained
//! publishes of the latest version, the coalescing of quick changes, the republish after a
//! broker restart and its job lock, the readiness of the connection, and the apply reports.
//!
//! The delivery runs in the test's thread (the default current-thread runtime), so its log
//! events are captured. The consumer that stores versions here has no delivery: each test
//! announces the versions itself, or relies on the republish, so another test's consumer can never
//! announce to this test's delivery. The binary's own announcement from its consumer is in
//! `tests/startup.rs`.
//!
//! The tests run one at a time ([`SERIAL`]): they share the republish job's lock in the binary's
//! Postgres, and a test that expects its delivery to republish must not find another test's
//! delivery holding it.
//!
//! Starts a throwaway Postgres with TLS and a Mosquitto (podman or docker); skipped without
//! either unless `CI` is set.

mod support;

use std::time::Duration;

use contract::{DeviceId, DeviceModelId, GatewayBindingId, ServiceName, TenantId};
use gateway_ops::config::MIN_DATABASE_CONNECTIONS;
use gateway_ops::delivery::records::Records;
use gateway_ops::delivery::{
    Delivery, DeliveryHandle, LOCK_SERVICE, Parts, REPUBLISH_JOB, publish_lock_key,
};
use gateway_ops::desired::DesiredConfigs;
use protocol::config::{ConfigReport, DeviceModel};
use protocol::{ConfigVersion, GatewayId, Issue, IssueCode};
use service_kit::identity::ServiceIdentity;
use service_kit::job_lock::JobLock;
use service_kit::tenant::TenantDb;
use sqlx::PgConnection;
use sqlx::postgres::PgPoolOptions;
use support::broker::{Broker, Gateway};
use support::{
    Assets, Capture, LOGINS, consumer, deliver, device, hardware_id, model, platform, versions,
    wait_for,
};
use testkit::product::{DATABASE, ProductPostgres, product_postgres};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// One test at a time; see the module.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// How long the delivery may take to connect, or to republish, on a loaded host.
const CONNECT_LIMIT: Duration = Duration::from_secs(120);

/// A version.
fn version(value: u64) -> ConfigVersion {
    ConfigVersion::new(value).expect("versions start at 1")
}

/// A delivery running in the test's thread until [`Running::stop`].
struct Running {
    handle: DeliveryHandle,
    stop: CancellationToken,
    task: JoinHandle<()>,
}

impl Running {
    /// Starts a delivery against `product` and `broker` with the coalescing window `throttle`,
    /// as `gateway_ops::start` does, and waits until it is connected and its first republish
    /// ran (or was skipped, when `lock_held`).
    async fn start(
        product: &ProductPostgres,
        broker: &Broker,
        throttle: Duration,
        lock_held: bool,
    ) -> Self {
        let (db, background) = TenantDb::connect_lazy_with_background(
            product.postgres.options("gateway_ops_app", DATABASE),
            // The smallest pool the settings accept: the delivery must work within it.
            PgPoolOptions::new().max_connections(MIN_DATABASE_CONNECTIONS),
            ServiceIdentity::new(ServiceName::GatewayOps),
        )
        .expect("the gateway-ops service role's pool");
        let delivery = Delivery::start(Parts {
            mqtt: broker.service_config(),
            configs: DesiredConfigs::new(&platform(product)),
            records: Records::new(background),
            locks: db.pool().clone(),
            throttle,
        })
        .expect("start the delivery");
        let handle = delivery.handle();
        let stop = CancellationToken::new();
        let task = tokio::spawn(delivery.run(stop.clone()));
        let running = Self { handle, stop, task };
        running.wait_ready().await;
        let what = if lock_held {
            "the first republish skipped for the held lock"
        } else {
            "the first republish"
        };
        running
            .wait_counts(what, |counts| {
                if lock_held {
                    counts.republish_skipped >= 1
                } else {
                    counts.republish_runs >= 1
                }
            })
            .await;
        running
    }

    async fn wait_ready(&self) {
        let connection = self.handle.connection();
        wait_for(
            "the delivery connected with the report subscription granted",
            CONNECT_LIMIT,
            || {
                let ready = connection.check().is_ok();
                async move { ready.then_some(()) }
            },
        )
        .await;
    }

    async fn wait_counts(
        &self,
        what: &str,
        done: impl Fn(&gateway_ops::delivery::Counts) -> bool,
    ) -> gateway_ops::delivery::Counts {
        wait_for(what, CONNECT_LIMIT, || {
            let counts = self.handle.counts();
            let found = done(&counts).then_some(counts);
            async move { found }
        })
        .await
    }

    async fn stop(self) {
        self.stop.cancel();
        self.task
            .await
            .expect("the delivery ends without panicking");
    }
}

/// A gateway bound to a tenant with one device: versions 1 (the binding) and 2 (the device).
struct Bound {
    tenant: TenantId,
    gateway: GatewayId,
    binding: GatewayBindingId,
    model_id: DeviceModelId,
    model: DeviceModel,
    device_id: DeviceId,
    revision: u64,
}

impl Bound {
    async fn new(product: &ProductPostgres, prefix: &str) -> Self {
        let (consumer, _) = consumer(product).await;
        let assets = Assets::new(product);
        let tenant = TenantId::generate();
        let gateway = hardware_id(prefix);
        let (binding, bound) = assets.bind(tenant, &gateway).await;
        let meter = model("meter");
        let (model_id, model_event) = assets.create_model(tenant, &meter).await;
        let device_id = DeviceId::generate();
        let created = assets
            .create_device(
                tenant,
                &gateway,
                model_id,
                Some(binding),
                device_id,
                &device(device_id, &meter, 1),
            )
            .await;
        deliver(product, &consumer, &[bound, model_event, created]).await;
        Self {
            tenant,
            gateway,
            binding,
            model_id,
            model: meter,
            device_id,
            revision: 1,
        }
    }

    /// Changes the device's unit ID: the next version.
    async fn change(&mut self, product: &ProductPostgres) {
        let (consumer, _) = consumer(product).await;
        self.revision += 1;
        let unit = u8::try_from(self.revision % 200 + 1).expect("a unit ID");
        let event = Assets::new(product)
            .update_device(
                self.tenant,
                &self.gateway,
                self.model_id,
                Some(self.binding),
                self.device_id,
                self.revision,
                &device(self.device_id, &self.model, unit),
            )
            .await;
        deliver(product, &consumer, &[event]).await;
    }

    async fn latest(&self, product: &ProductPostgres) -> ConfigVersion {
        let mut admin = product.superuser().await;
        let stored = versions(&mut admin, &self.gateway).await;
        let last = stored.last().expect("a stored version").version;
        version(u64::try_from(last).expect("a positive version"))
    }
}

/// A row of `gateway_ops.config_deliveries`.
#[derive(Debug, PartialEq, Eq)]
struct DeliveryRow {
    tenant_id: String,
    version: i64,
    published: bool,
    republished: bool,
    outcome: Option<String>,
    active_version: Option<i64>,
    issues: Option<String>,
}

const DELIVERIES: &str = "SELECT tenant_id::text, version,
    first_published_at IS NOT NULL AS published,
    last_published_at > first_published_at AS republished,
    outcome, active_version, issues::text
FROM gateway_ops.config_deliveries WHERE hardware_id = $1 ORDER BY version";

async fn deliveries(admin: &mut PgConnection, gateway: &GatewayId) -> Vec<DeliveryRow> {
    type Row = (
        String,
        i64,
        bool,
        Option<bool>,
        Option<String>,
        Option<i64>,
        Option<String>,
    );
    let rows: Vec<Row> = sqlx::query_as(DELIVERIES)
        .bind(gateway.as_str())
        .fetch_all(admin)
        .await
        .expect("read the deliveries");
    rows.into_iter()
        .map(
            |(tenant_id, version, published, republished, outcome, active_version, issues)| {
                DeliveryRow {
                    tenant_id,
                    version,
                    published,
                    republished: republished.unwrap_or(false),
                    outcome,
                    active_version,
                    issues,
                }
            },
        )
        .collect()
}

#[tokio::test]
async fn ten_quick_changes_are_published_once_retained_as_the_latest_version() {
    const THROTTLE: Duration = Duration::from_secs(10);
    let _serial = SERIAL.lock().await;
    let Some(product) = product_postgres(LOGINS).await else {
        return;
    };
    let Some(broker) = Broker::start() else {
        return;
    };
    let running = Running::start(&product, &broker, THROTTLE, false).await;
    let mut listener = Gateway::connect(&broker).await;

    // Stored after the first republish, so only the announcements below publish it.
    let mut bound = Bound::new(&product, "go-coalesce").await;
    for _ in 0..9 {
        bound.change(&product).await;
    }
    let latest = bound.latest(&product).await;
    assert_eq!(latest.get(), 11, "the binding, the device and nine changes");

    let before = running.handle.counts();
    let announced = tokio::time::Instant::now();
    for value in 2..=latest.get() {
        running.handle.changed(&bound.gateway, version(value));
    }
    let after = running.handle.counts();
    assert_eq!(
        after.coalesced - before.coalesced,
        9,
        "ten announcements within the throttle make one entry"
    );

    let first = listener
        .wait("the coalesced publish", &bound.gateway, |received| {
            received.version.is_some()
        })
        .await;
    let waited = announced.elapsed();
    assert_eq!(
        first.version,
        Some(latest),
        "the first publish is the latest version"
    );
    assert!(
        waited >= THROTTLE,
        "the publish waited for the throttle that the first announcement opened ({waited:?})"
    );
    // Absence can only be sampled: nothing else for this gateway within another throttle.
    listener.drain(THROTTLE).await;
    let published = listener.of(&bound.gateway);
    assert_eq!(
        published.len(),
        1,
        "one publish for ten changes: {published:?}"
    );
    assert_eq!(running.handle.counts().published - before.published, 1);
    assert!(running.handle.is_idle(&bound.gateway));

    // A new subscriber gets the latest version retained.
    let newcomer_received = {
        let mut newcomer = Gateway::connect(&broker).await;
        let retained = newcomer
            .wait("the retained configuration", &bound.gateway, |received| {
                received.retain
            })
            .await;
        newcomer.close().await;
        retained
    };
    assert_eq!(newcomer_received.version, Some(latest));
    assert_eq!(newcomer_received.qos, 1);

    // The service records the publish after the PUBACK, which the subscriber does not wait for.
    let rows = wait_for("the publish recorded", CONNECT_LIMIT, || {
        let (product, gateway) = (&product, &bound.gateway);
        async move {
            let mut admin = product.superuser().await;
            let rows = deliveries(&mut admin, gateway).await;
            (!rows.is_empty()).then_some(rows)
        }
    })
    .await;
    assert_eq!(
        rows,
        [DeliveryRow {
            tenant_id: bound.tenant.to_string(),
            version: 11,
            published: true,
            republished: false,
            outcome: None,
            active_version: None,
            issues: None,
        }],
        "only the published version is recorded, in its tenant"
    );
    running.stop().await;
    println!(
        "evidence: 10 announcements (versions 2..=11) within a {THROTTLE:?} throttle -> \
         coalesced {}, published {} (version {}) after {waited:?}; a new subscriber got \
         version {:?} retained with QoS {}; config_deliveries: {rows:?}",
        after.coalesced - before.coalesced,
        published.len(),
        latest.get(),
        newcomer_received.version.map(ConfigVersion::get),
        newcomer_received.qos
    );
}

#[tokio::test]
async fn a_broker_restart_republishes_every_gateway_and_readiness_follows_the_connection() {
    let _serial = SERIAL.lock().await;
    let Some(product) = product_postgres(LOGINS).await else {
        return;
    };
    let Some(broker) = Broker::start() else {
        return;
    };
    // Stored before the delivery starts: only the republish publishes them.
    let first = Bound::new(&product, "go-restart-a").await;
    let mut second = Bound::new(&product, "go-restart-b").await;
    second.change(&product).await;
    let (first_latest, second_latest) =
        (first.latest(&product).await, second.latest(&product).await);
    let running = Running::start(&product, &broker, Duration::from_secs(1), false).await;
    let mut listener = Gateway::connect(&broker).await;
    for bound in [&first, &second] {
        let latest = bound.latest(&product).await;
        listener
            .wait(
                "the republish at the first connection",
                &bound.gateway,
                |received| received.retain && received.version == Some(latest),
            )
            .await;
    }
    listener.close().await;

    broker.stop();
    let connection = running.handle.connection();
    wait_for(
        "readiness failing without the broker",
        CONNECT_LIMIT,
        || {
            let failing = connection.check().is_err();
            async move { failing.then_some(()) }
        },
    )
    .await;
    let not_ready = connection
        .check()
        .expect_err("not ready without the broker")
        .to_string();
    let runs_before = running.handle.counts().republish_runs;

    broker.resume();
    running.wait_ready().await;
    let counts = running
        .wait_counts("the republish after the restart", |counts| {
            counts.republish_runs > runs_before
        })
        .await;
    // The restarted broker kept no retained message: these come from the republish.
    let mut newcomer = Gateway::connect(&broker).await;
    let mut republished = Vec::new();
    for (bound, latest) in [(&first, first_latest), (&second, second_latest)] {
        let received = newcomer
            .wait(
                "the republished configuration",
                &bound.gateway,
                |received| received.retain,
            )
            .await;
        assert_eq!(received.version, Some(latest));
        republished.push((bound.gateway.to_string(), latest.get()));
    }
    newcomer.close().await;

    let mut admin = product.superuser().await;
    for bound in [&first, &second] {
        let rows = deliveries(&mut admin, &bound.gateway).await;
        let latest = rows.last().expect("the latest version's row");
        assert!(latest.published && latest.republished, "{rows:?}");
    }
    running.stop().await;
    println!(
        "evidence: readiness without the broker: {not_ready:?}; after the restart: ready, \
         republish runs {runs_before} -> {}, republished publishes {}, a new subscriber got \
         {republished:?} retained",
        counts.republish_runs, counts.republished
    );
}

#[tokio::test]
async fn a_replica_finding_the_republish_lock_taken_leaves_the_republish_to_its_holder() {
    let _serial = SERIAL.lock().await;
    let Some(product) = product_postgres(LOGINS).await else {
        return;
    };
    let Some(broker) = Broker::start() else {
        return;
    };
    let bound = Bound::new(&product, "go-lock").await;
    let (db, _) = TenantDb::connect_lazy_with_background(
        product.postgres.options("gateway_ops_app", DATABASE),
        PgPoolOptions::new().max_connections(2),
        ServiceIdentity::new(ServiceName::GatewayOps),
    )
    .expect("the gateway-ops service role's pool");
    // Another replica republishing: holds the lock until released.
    let (held_tx, held) = tokio::sync::oneshot::channel();
    let (release, released) = tokio::sync::oneshot::channel::<()>();
    let holder = tokio::spawn(async move {
        let lock = JobLock::new(db.pool().clone(), LOCK_SERVICE, REPUBLISH_JOB);
        lock.run_exclusive(|| async move {
            let _ = held_tx.send(());
            let _ = released.await;
        })
        .await
        .expect("take the republish lock")
        .expect("the lock was free");
    });
    held.await.expect("the lock is held");

    let running = Running::start(&product, &broker, Duration::from_secs(1), true).await;
    let counts = running.handle.counts();
    assert_eq!(counts.republish_runs, 0, "{counts:?}");
    assert_eq!(counts.republished, 0, "{counts:?}");
    let mut listener = Gateway::connect(&broker).await;
    // Absence can only be sampled: the skipping replica published nothing.
    listener.drain(Duration::from_secs(2)).await;
    assert!(listener.of(&bound.gateway).is_empty());
    listener.close().await;
    let _ = release.send(());
    holder.await.expect("the holder ends");
    running.stop().await;
    println!(
        "evidence: with the lock {LOCK_SERVICE}/{REPUBLISH_JOB} held elsewhere: republish \
         skipped {}, runs {}, republished {}",
        counts.republish_skipped, counts.republish_runs, counts.republished
    );
}

#[tokio::test]
async fn a_gateway_another_replica_is_publishing_waits_for_its_publish_lock() {
    const THROTTLE: Duration = Duration::from_secs(1);
    let _serial = SERIAL.lock().await;
    let Some(product) = product_postgres(LOGINS).await else {
        return;
    };
    let Some(broker) = Broker::start() else {
        return;
    };
    let running = Running::start(&product, &broker, THROTTLE, false).await;
    let mut listener = Gateway::connect(&broker).await;
    // Stored after the first republish, so only the announcement below publishes it.
    let bound = Bound::new(&product, "go-publish-lock").await;
    let latest = bound.latest(&product).await;

    // Another replica reading and publishing this gateway holds its lock.
    let mut other = product.superuser().await;
    sqlx::query("BEGIN")
        .execute(&mut other)
        .await
        .expect("begin the other replica's transaction");
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(publish_lock_key(&bound.gateway))
        .execute(&mut other)
        .await
        .expect("take the gateway's publish lock");
    running.handle.changed(&bound.gateway, latest);
    // Absence can only be sampled: nothing while the lock is held, over several throttles.
    listener.drain(THROTTLE * 5).await;
    let while_held = listener.of(&bound.gateway);
    assert!(while_held.is_empty(), "{while_held:?}");
    assert!(
        !running.handle.is_idle(&bound.gateway),
        "the change still waits"
    );

    sqlx::query("COMMIT")
        .execute(&mut other)
        .await
        .expect("release the gateway's publish lock");
    let published = listener
        .wait(
            "the publish after the lock was released",
            &bound.gateway,
            |received| received.version.is_some(),
        )
        .await;
    assert_eq!(published.version, Some(latest));
    running.stop().await;
    println!(
        "evidence: while another session held the publish lock of the gateway: {} publishes \
         over {:?}; after it released the lock: version {:?} published",
        while_held.len(),
        THROTTLE * 5,
        published.version.map(ConfigVersion::get)
    );
}

/// An apply report with fields a gateway would not send, naming another gateway.
#[derive(serde::Serialize)]
struct Spoofed<'a> {
    v: u16,
    version: u64,
    outcome: &'a str,
    active_version: u64,
    gateway: &'a str,
    hardware_id: &'a str,
}

fn encode(report: &ConfigReport) -> Vec<u8> {
    report.encode().expect("encode the report")
}

#[tokio::test]
async fn apply_reports_are_recorded_under_the_topics_gateway_and_unreasonable_ones_refused() {
    let _serial = SERIAL.lock().await;
    let Some(product) = product_postgres(LOGINS).await else {
        return;
    };
    let Some(broker) = Broker::start() else {
        return;
    };
    let (capture, _guard) = Capture::install();
    let mut bound = Bound::new(&product, "go-report").await;
    let other = Bound::new(&product, "go-report-other").await;
    let running = Running::start(&product, &broker, Duration::from_secs(1), false).await;
    let mut gateway = Gateway::connect(&broker).await;
    let v2 = bound.latest(&product).await;
    assert_eq!(v2.get(), 2);
    gateway
        .wait("the republish", &bound.gateway, |received| {
            received.version == Some(v2)
        })
        .await;
    let mut admin = product.superuser().await;

    let mut expect_recorded = running.handle.counts().reports_recorded;
    let mut expect_refused = running.handle.counts().reports_refused;

    // Rejected, with an issue: recorded with the issue.
    let issue = Issue::new(
        IssueCode::OutOfRange,
        "devices[0].unit_id",
        "unit ID out of range",
    );
    gateway
        .report(
            &bound.gateway,
            encode(&ConfigReport::rejected(v2, Some(version(1)), vec![issue])),
        )
        .await;
    expect_recorded += 1;
    running
        .wait_counts("the rejected report recorded", |counts| {
            counts.reports_recorded >= expect_recorded
        })
        .await;
    let rows = deliveries(&mut admin, &bound.gateway).await;
    let row = rows
        .iter()
        .find(|row| row.version == 2)
        .expect("version 2's row");
    assert_eq!(row.outcome.as_deref(), Some("rejected"));
    assert_eq!(row.active_version, Some(1));
    assert!(
        row.issues.as_deref().is_some_and(
            |issues| issues.contains("out_of_range") && issues.contains("devices[0].unit_id")
        ),
        "{row:?}"
    );

    // Applied, but the payload names the other gateway: the topic's gateway is the one recorded.
    let spoofed = Spoofed {
        v: 1,
        version: 2,
        outcome: "applied",
        active_version: 2,
        gateway: other.gateway.as_str(),
        hardware_id: other.gateway.as_str(),
    };
    gateway
        .report(
            &bound.gateway,
            cbor4ii::serde::to_vec(Vec::new(), &spoofed).expect("encode the spoofed report"),
        )
        .await;
    expect_recorded += 1;
    running
        .wait_counts("the spoofed report recorded", |counts| {
            counts.reports_recorded >= expect_recorded
        })
        .await;
    let rows = deliveries(&mut admin, &bound.gateway).await;
    let row = rows
        .iter()
        .find(|row| row.version == 2)
        .expect("version 2's row");
    assert_eq!(row.outcome.as_deref(), Some("applied"));
    assert_eq!(row.active_version, Some(2));
    assert_eq!(row.tenant_id, bound.tenant.to_string());
    let other_rows = deliveries(&mut admin, &other.gateway).await;
    assert!(
        other_rows.iter().all(|row| row.outcome.is_none()),
        "the gateway named in the payload has no report: {other_rows:?}"
    );

    // Unreasonable reports are refused and change nothing.
    let unknown = hardware_id("go-report-unknown");
    let refusals: [(&str, &GatewayId, Vec<u8>); 4] = [
        (
            "a version above the latest",
            &bound.gateway,
            encode(&ConfigReport::applied(version(99))),
        ),
        (
            "an active version above the latest",
            &bound.gateway,
            encode(&ConfigReport::rejected(v2, Some(version(50)), Vec::new())),
        ),
        (
            "a gateway without configurations",
            &unknown,
            encode(&ConfigReport::applied(version(1))),
        ),
        (
            "a malformed payload",
            &bound.gateway,
            vec![0xff, 0x00, 0x13],
        ),
    ];
    for (what, target, payload) in refusals {
        gateway.report(target, payload).await;
        expect_refused += 1;
        running
            .wait_counts(what, |counts| counts.reports_refused >= expect_refused)
            .await;
    }
    let reasons: Vec<String> = capture
        .messages("apply report refused")
        .into_iter()
        .map(|fields| fields["reason"].clone())
        .collect();
    assert_eq!(
        reasons,
        [
            "version_ahead",
            "version_ahead",
            "unknown_gateway",
            "decode"
        ]
    );
    let rows_after = deliveries(&mut admin, &bound.gateway).await;
    assert_eq!(rows_after, rows, "refused reports change nothing");
    assert_eq!(running.handle.counts().reports_recorded, expect_recorded);

    // A report about an older version than the latest: recorded, and the latest is published
    // again (the consumer here has no delivery, so nothing else publishes version 3).
    bound.change(&product).await;
    let v3 = bound.latest(&product).await;
    assert_eq!(v3.get(), 3);
    gateway
        .report(&bound.gateway, encode(&ConfigReport::applied(v2)))
        .await;
    let published = gateway
        .wait(
            "the latest after a stale report",
            &bound.gateway,
            |received| received.version == Some(v3),
        )
        .await;
    // The subscriber can see the publish before the service records it after the PUBACK.
    let rows = wait_for("version 3's publish recorded", CONNECT_LIMIT, || {
        let (product, gateway) = (&product, &bound.gateway);
        async move {
            let mut admin = product.superuser().await;
            let rows = deliveries(&mut admin, gateway).await;
            rows.iter()
                .any(|row| row.version == 3 && row.published)
                .then_some(rows)
        }
    })
    .await;

    // No log line carries the broker password.
    let password = std::fs::read_to_string(broker.service_config().password_file)
        .expect("read the password file");
    assert!(!capture.text().contains(password.trim()));
    let recorded_logs = capture.messages("apply report recorded");
    running.stop().await;
    println!(
        "evidence: recorded {:?}; refused reasons {reasons:?}; stale report of version 2 -> \
         version {:?} published again; config_deliveries: {rows:?}",
        recorded_logs
            .iter()
            .map(|fields| (fields["hardware_id"].clone(), fields["outcome"].clone()))
            .collect::<Vec<_>>(),
        published.version.map(ConfigVersion::get)
    );
}

#[tokio::test]
async fn a_gateway_flooding_its_report_topic_does_not_crowd_out_another() {
    /// Reports from one gateway, each about a version never generated.
    const FLOOD: u64 = 500;
    let _serial = SERIAL.lock().await;
    let Some(product) = product_postgres(LOGINS).await else {
        return;
    };
    let Some(broker) = Broker::start() else {
        return;
    };
    let flooder = Bound::new(&product, "go-flood").await;
    let quiet = Bound::new(&product, "go-flood-quiet").await;
    let quiet_latest = quiet.latest(&product).await;
    let running = Running::start(&product, &broker, Duration::from_secs(1), false).await;
    let gateway = Gateway::connect(&broker).await;
    let before = running.handle.counts();

    for value in 1..=FLOOD {
        gateway
            .report(
                &flooder.gateway,
                encode(&ConfigReport::applied(version(1_000 + value))),
            )
            .await;
    }
    gateway
        .report(&quiet.gateway, encode(&ConfigReport::applied(quiet_latest)))
        .await;

    // The flooding gateway held one place in the inbox: the other gateway's report is recorded,
    // and each flood report was either refused or replaced by the flooder's next one.
    running
        .wait_counts("the other gateway's report recorded", |counts| {
            counts.reports_recorded > before.reports_recorded
        })
        .await;
    let counts = running
        .wait_counts("every flood report refused or superseded", |counts| {
            (counts.reports_refused - before.reports_refused)
                + (counts.reports_superseded - before.reports_superseded)
                >= FLOOD
        })
        .await;
    let mut admin = product.superuser().await;
    let rows = deliveries(&mut admin, &quiet.gateway).await;
    let row = rows
        .iter()
        .find(|row| row.version == i64::try_from(quiet_latest.get()).expect("fits"))
        .expect("the other gateway's latest version");
    assert_eq!(row.outcome.as_deref(), Some("applied"));
    assert_eq!(counts.reports_lost, before.reports_lost);
    running.stop().await;
    println!(
        "evidence: {FLOOD} reports of versions never generated from one gateway, then one from \
         another: the other's recorded ({:?}); of the flood {} refused after a read and {} \
         superseded in the inbox; none lost",
        row.outcome,
        counts.reports_refused - before.reports_refused,
        counts.reports_superseded - before.reports_superseded
    );
}
