//! The telemetry path (Laysi/trustiot-v2#641; services/ingest/README.md, "寫入流程") against a
//! real broker, ClickHouse and Postgres: the pipeline of `src/telemetry` runs in this process, so
//! the test can hold a batch at a step of the write path or end the pipeline there
//! (`telemetry::hooks`), and run two replicas side by side. The stand-in gateway publishes
//! batches and reads the acknowledgements through the broker (tests/support/broker.rs).
//!
//! One scenario on one set of containers, step by step:
//!
//! 1. A batch is committed once: `committed` with its record count, then `duplicate` with the
//!    same count and the same rows; the tenant is the binding's, not one the payload names.
//! 2. A gateway bound to no tenant: `retry`, nothing written. A batch the table cannot hold
//!    (a device that is not a UUID) and a payload that decodes only as far as its header:
//!    `rejected`. A payload without even a header: no answer, logged.
//! 3. A ClickHouse failure (the completion table refuses the insert): `retry` with the rows
//!    written and no completion record; once it accepts again, the resent batch is rewritten
//!    (the rows of the first attempt deleted) and `committed`, the rows there once. The same with
//!    the gateway rebound to another tenant between the two attempts: the batch ends up under
//!    the new tenant only.
//! 4. A retained batch: the broker replays it to a new subscription without a share group with
//!    the retain flag, and the pipeline drops it and logs it.
//! 5. The pipeline ends between the rows and the completion record (held there by a hook, then
//!    aborted): the resent batch is rewritten by a new pipeline with the same receive time, so
//!    the deleted block and the new one are identical, and the new one lands; no `committed`
//!    goes out before the completion record is on disk.
//! 6. Two replicas in different share groups get every batch: while one holds a stream's lock
//!    the other answers `retry`; batch after batch, exactly one commits, the table holds each
//!    once.
//! 7. Over everything written: both aggregate tables equal the raw rows, every statement of the
//!    service's account carried the stream settings, every completion record is no earlier than
//!    its batch's rows were received, and what the leftover search and the DELETE read next to a
//!    table with background rows of other devices.
//!
//! Starts throwaway containers with podman or docker; skipped without either unless `CI` is set.
//! Run with `--nocapture` to see the evidence lines.
#![allow(
    clippy::print_stdout,
    reason = "prints the evidence lines the test report quotes"
)]

mod common;
mod support;

use std::io::Write as _;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{Account, ClickHouse};
use ingest::bindings::GatewayBindings;
use ingest::telemetry::hooks::{Checkpoint, HookFuture, Hooks, Point};
use ingest::telemetry::{
    BUSY_RETRY_AFTER_MS, Parts, Pipeline, RETRY_AFTER_MS, Subscription, UNBOUND_RETRY_AFTER_MS,
};
use protocol::telemetry::AckOutcome;
use protocol::{GatewayId, IssueCode, StreamId};
use service_kit::tenant::{PlatformDb, TenantDb};
use sqlx::postgres::PgPoolOptions;
use support::broker::{ACK_WAIT, Broker, Gateway, batch, now_millis};
use support::{START_LIMIT, bind, wait_for};
use testkit::product::{DATABASE, ProductPostgres, product_postgres};
use tokio::sync::{Semaphore, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// The service's ClickHouse account in this test, with the service's role.
const USER: &str = "telemetry_test_ingest";
const CREATE_ACCOUNT: &str = "CREATE USER IF NOT EXISTS telemetry_test_ingest
IDENTIFIED WITH sha256_password BY {password:String} HOST ANY";
const GRANT_ROLE: &str = "GRANT trustiot_ingest TO telemetry_test_ingest";

/// The account the test reads with: the service's role too (the admin reads no telemetry row),
/// but another user, so the query log tells the test's reads from the service's statements.
const READER: &str = "telemetry_test_reader";
const CREATE_READER: &str = "CREATE USER IF NOT EXISTS telemetry_test_reader
IDENTIFIED WITH sha256_password BY {password:String} HOST ANY";
const GRANT_READER: &str = "GRANT trustiot_ingest TO telemetry_test_reader";

/// A ClickHouse failure on the last step: the completion table refuses the role's inserts.
const REVOKE_COMMIT: &str = "REVOKE INSERT ON trustiot.stream_commits FROM trustiot_ingest";
const GRANT_COMMIT: &str = "GRANT INSERT ON trustiot.stream_commits TO trustiot_ingest";

/// The rows of one batch, and their tenants.
const BATCH_ROWS: &str = "SELECT count() FROM trustiot.telemetry
WHERE gateway_id = {gateway:String} AND stream_id = {stream:String} AND batch_seq = {seq:UInt64}
FORMAT TabSeparated";
const BATCH_TENANTS: &str = "SELECT DISTINCT toString(tenant_id) FROM trustiot.telemetry
WHERE gateway_id = {gateway:String} AND stream_id = {stream:String} AND batch_seq = {seq:UInt64}
ORDER BY 1 FORMAT TabSeparated";

/// The completion records of one batch, with their tenant and binding.
const BATCH_RECORDS: &str =
    "SELECT toString(tenant_id), toString(binding_id), row_count FROM trustiot.stream_commits
WHERE gateway_id = {gateway:String} AND stream_id = {stream:String} AND seq = {seq:UInt64}
FORMAT TabSeparated";

/// Background rows of other devices of another tenant, received now, so the leftover search and
/// the DELETE have something to skip: `rows` readings over 500 devices and the last day.
const BACKGROUND: &str = "INSERT INTO trustiot.telemetry
    (tenant_id, gateway_id, stream_id, batch_seq, device_id, point, ts, received_at, value_type,
     value_float)
SELECT {tenant:UUID}, 'gw-background', 'background', intDiv(number, 1000) + 1,
    toUUID(concat('00000000-0000-4000-8000-', leftPad(toString(number % 500), 12, '0'))),
    'power', now64(3, 'UTC') - toIntervalMillisecond(number * 997 % 86400000), now64(3, 'UTC'),
    'float', number
FROM numbers({rows:UInt64})";
const TABLE_ROWS: &str = "SELECT count() FROM trustiot.telemetry FORMAT TabSeparated";

/// Raw rows against one aggregate table, cell by cell, for the given gateways: the cells that
/// differ in readings or sum, and how many cells there are.
const RAW_VS_1M: &str = "SELECT countIf(r.raw_readings != a.agg_readings
        OR abs(r.raw_sum - a.agg_sum) > 1e-6 * greatest(abs(r.raw_sum), 1)), count()
FROM
(
    SELECT tenant_id, device_id, point, toStartOfMinute(ts) AS bucket, count() AS raw_readings,
        sum(multiIf(value_type = 'float', value_float, value_type = 'int', toFloat64(value_int),
            0)) AS raw_sum
    FROM trustiot.telemetry WHERE has({gateways:Array(String)}, gateway_id)
    GROUP BY tenant_id, device_id, point, bucket
) AS r
FULL OUTER JOIN
(
    SELECT tenant_id, device_id, point, bucket, sum(readings) AS agg_readings,
        sum(value_sum) AS agg_sum
    FROM trustiot.telemetry_1m WHERE has({gateways:Array(String)}, gateway_id)
    GROUP BY tenant_id, device_id, point, bucket
    HAVING agg_readings != 0
) AS a USING (tenant_id, device_id, point, bucket)
FORMAT TabSeparated";
const RAW_VS_1H: &str = "SELECT countIf(r.raw_readings != a.agg_readings
        OR abs(r.raw_sum - a.agg_sum) > 1e-6 * greatest(abs(r.raw_sum), 1)), count()
FROM
(
    SELECT tenant_id, device_id, point, toStartOfHour(ts) AS bucket, count() AS raw_readings,
        sum(multiIf(value_type = 'float', value_float, value_type = 'int', toFloat64(value_int),
            0)) AS raw_sum
    FROM trustiot.telemetry WHERE has({gateways:Array(String)}, gateway_id)
    GROUP BY tenant_id, device_id, point, bucket
) AS r
FULL OUTER JOIN
(
    SELECT tenant_id, device_id, point, bucket, sum(readings) AS agg_readings,
        sum(value_sum) AS agg_sum
    FROM trustiot.telemetry_1h WHERE has({gateways:Array(String)}, gateway_id)
    GROUP BY tenant_id, device_id, point, bucket
    HAVING agg_readings != 0
) AS a USING (tenant_id, device_id, point, bucket)
FORMAT TabSeparated";

/// Per batch: the earliest completion record against the latest receive time of its rows, in
/// milliseconds; the smallest difference and how many batches.
const COMMITTED_AFTER_RECEIVED: &str =
    "SELECT min(dateDiff('millisecond', r.received, c.committed)), count()
FROM
(
    SELECT gateway_id, stream_id, seq, min(committed_at) AS committed
    FROM trustiot.stream_commits GROUP BY gateway_id, stream_id, seq
) AS c
INNER JOIN
(
    SELECT gateway_id, stream_id, batch_seq AS seq, max(received_at) AS received
    FROM trustiot.telemetry WHERE gateway_id != 'gw-background'
    GROUP BY gateway_id, stream_id, seq
) AS r USING (gateway_id, stream_id, seq)
FORMAT TabSeparated";

const FLUSH_LOGS: &str = "SYSTEM FLUSH LOGS";

/// Every statement the service's account started, and those that carried the stream settings.
const STATEMENT_SETTINGS: &str = "SELECT query_kind, count(),
    countIf(startsWith(query_id, 'trustiot-rewrite/')
        AND Settings['replace_running_query'] = '1'
        AND Settings['max_execution_time'] = '20'
        AND Settings['async_insert'] = '0')
FROM system.query_log
WHERE user = {user:String} AND type = 'QueryStart'
GROUP BY query_kind ORDER BY query_kind FORMAT TabSeparated";

/// What the leftover search and the DELETE read.
const LEFTOVER_READS: &str = "SELECT query_kind, count(), max(read_rows), max(read_bytes)
FROM system.query_log
WHERE user = {user:String} AND type = 'QueryFinish'
    AND (query_kind = 'Delete' OR startsWith(query, 'SELECT DISTINCT tenant_id'))
GROUP BY query_kind ORDER BY query_kind FORMAT TabSeparated";

/// The tenant column's comment (0013_tenant_id_comment.sql).
const TENANT_COMMENT: &str = "SELECT comment FROM system.columns
WHERE database = 'trustiot' AND table = 'telemetry' AND name = 'tenant_id' FORMAT TabSeparated";

/// The advisory locks held on the database, by any session.
const ADVISORY_LOCKS: &str = "SELECT count(*) FROM pg_locks WHERE locktype = 'advisory'";

/// How long to sample for an answer that must not come: absence can only be sampled, so this
/// can miss a wrong answer but never fail a right one.
const ABSENCE_WINDOW: Duration = Duration::from_secs(3);

/// Log lines of this process, kept for the assertions on what the pipeline logged.
#[derive(Clone, Default)]
struct Logs(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Logs {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("log lock").extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Logs {
    fn install() -> Self {
        let logs = Self::default();
        let writer = logs.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::set_global_default(subscriber).expect("one subscriber per process");
        logs
    }

    fn lines(&self) -> Vec<String> {
        String::from_utf8_lossy(&self.0.lock().expect("log lock"))
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// The lines that contain every one of `parts`.
    fn matching(&self, parts: &[&str]) -> Vec<String> {
        self.lines()
            .into_iter()
            .filter(|line| parts.iter().all(|part| line.contains(part)))
            .collect()
    }

    async fn wait_for(&self, parts: &[&str]) -> String {
        wait_for(
            &format!("a log line with {parts:?}"),
            START_LIMIT,
            || async { self.matching(parts).into_iter().next() },
        )
        .await
    }
}

/// A hold at one checkpoint of one batch: the hook reports it and waits for a release.
struct Hold {
    point: Point,
    seq: u64,
    reached: mpsc::UnboundedSender<Checkpoint>,
    release: Arc<Semaphore>,
}

/// The test's hooks: holds, and a fixed receive time.
#[derive(Default)]
struct TestHooks {
    holds: Vec<Hold>,
    received_at: Option<u64>,
}

impl Hooks for TestHooks {
    fn reached(&self, checkpoint: Checkpoint) -> HookFuture<'_> {
        Box::pin(async move {
            let Some(hold) = self
                .holds
                .iter()
                .find(|hold| hold.point == checkpoint.point && hold.seq == checkpoint.seq)
            else {
                return;
            };
            let _ = hold.reached.send(checkpoint);
            // Released by add_permits; a hold never released waits until the task is aborted.
            if let Ok(permit) = hold.release.acquire().await {
                permit.forget();
            }
        })
    }

    fn received_at(&self) -> Option<u64> {
        self.received_at
    }
}

/// A hold to put in [`TestHooks`] and the handles to watch and release it.
fn hold(point: Point, seq: u64) -> (Hold, mpsc::UnboundedReceiver<Checkpoint>, Arc<Semaphore>) {
    let (reached, watch) = mpsc::unbounded_channel();
    let release = Arc::new(Semaphore::new(0));
    (
        Hold {
            point,
            seq,
            reached,
            release: Arc::clone(&release),
        },
        watch,
        release,
    )
}

/// Waits for a hold to be reached.
async fn reached(watch: &mut mpsc::UnboundedReceiver<Checkpoint>, what: &str) -> Checkpoint {
    tokio::time::timeout(ACK_WAIT, watch.recv())
        .await
        .unwrap_or_else(|_| panic!("{what}: not reached within {ACK_WAIT:?}"))
        .expect("the hooks are alive")
}

/// A running pipeline.
struct Running {
    stop: CancellationToken,
    task: JoinHandle<()>,
}

impl Running {
    /// Stops it as the service does at shutdown, and waits until it ended.
    async fn stop(self) {
        self.stop.cancel();
        self.task
            .await
            .expect("the pipeline ended without panicking");
    }

    /// Ends it at once, as a crash would: the task and its batches are dropped where they are.
    async fn abort(self) {
        self.task.abort();
        let _ = self.task.await;
    }
}

struct Harness {
    product: Arc<ProductPostgres>,
    clickhouse: ClickHouse,
    broker: Broker,
    /// The test's own reads.
    reader: Account,
    /// The service's ClickHouse password, which must not appear in the log.
    service_password: String,
    service_clickhouse: ingest::clickhouse::ClickHouse,
    db: TenantDb,
    platform: PlatformDb,
}

impl Harness {
    async fn start() -> Option<Self> {
        let product = product_postgres(&["ingest_app", "ingest_platform"]).await?;
        let clickhouse = ClickHouse::start_throwaway().await?;
        let broker = Broker::start()?;
        clickhouse.apply_migrations().await;
        let password = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        clickhouse
            .query_with(
                &clickhouse.admin,
                CREATE_ACCOUNT,
                &[("param_password", &password)],
            )
            .await
            .unwrap_or_else(|error| panic!("create the service's account: {}", error.body));
        clickhouse.admin_ok(GRANT_ROLE).await;
        let reader_password = uuid::Uuid::new_v4().simple().to_string();
        clickhouse
            .query_with(
                &clickhouse.admin,
                CREATE_READER,
                &[("param_password", &reader_password)],
            )
            .await
            .unwrap_or_else(|error| panic!("create the test's account: {}", error.body));
        clickhouse.admin_ok(GRANT_READER).await;
        let service_clickhouse = ingest::clickhouse::ClickHouse::new(
            &clickhouse.url,
            None,
            ingest::clickhouse::Account::new(USER, &password),
        )
        .expect("the service's ClickHouse client");
        let db = TenantDb::connect_lazy(
            product.postgres.options("ingest_app", DATABASE),
            PgPoolOptions::new().max_connections(8),
        )
        .expect("verify-full options");
        let platform = PlatformDb::connect_lazy(
            product.postgres.options("ingest_platform", DATABASE),
            PgPoolOptions::new().max_connections(4),
        )
        .expect("verify-full options");
        Some(Self {
            product,
            clickhouse,
            broker,
            reader: Account {
                user: READER.to_owned(),
                password: reader_password,
            },
            service_password: password,
            service_clickhouse,
            db,
            platform,
        })
    }

    /// Starts a pipeline as the service does, with `subscription` and `hooks`, and waits until
    /// its subscription is granted.
    async fn pipeline(&self, subscription: Subscription, hooks: TestHooks) -> Running {
        let pipeline = Pipeline::start(Parts {
            clickhouse: self.service_clickhouse.clone(),
            bindings: GatewayBindings::new(&self.platform),
            locks: self.db.pool().clone(),
            mqtt: self.broker.ingest_config(),
            subscription,
            concurrency: 4,
            hooks: Arc::new(hooks),
        })
        .expect("start the pipeline");
        let connection = pipeline.connection();
        let stop = CancellationToken::new();
        let task = tokio::spawn(pipeline.run(stop.clone()));
        wait_for(
            "the pipeline's subscription being granted",
            START_LIMIT,
            || async { connection.is_up().then_some(()) },
        )
        .await;
        Running { stop, task }
    }

    /// Runs a read as the test's account, which has the service's role (the admin reads no
    /// telemetry row).
    async fn read(&self, sql: &str, params: &[(&str, &str)]) -> String {
        self.clickhouse
            .query_with(&self.reader, sql, params)
            .await
            .unwrap_or_else(|error| panic!("{sql}: {}", error.body))
    }

    async fn batch_rows(&self, gateway: &GatewayId, stream: &StreamId, seq: u64) -> u64 {
        let seq = seq.to_string();
        self.read(
            BATCH_ROWS,
            &[
                ("param_gateway", gateway.as_str()),
                ("param_stream", stream.as_str()),
                ("param_seq", &seq),
            ],
        )
        .await
        .trim()
        .parse()
        .expect("a count")
    }

    async fn batch_tenants(&self, gateway: &GatewayId, stream: &StreamId, seq: u64) -> Vec<String> {
        let seq = seq.to_string();
        self.read(
            BATCH_TENANTS,
            &[
                ("param_gateway", gateway.as_str()),
                ("param_stream", stream.as_str()),
                ("param_seq", &seq),
            ],
        )
        .await
        .lines()
        .map(str::to_owned)
        .collect()
    }

    /// The completion records of a batch: tenant, binding, row count.
    async fn batch_records(
        &self,
        gateway: &GatewayId,
        stream: &StreamId,
        seq: u64,
    ) -> Vec<(String, String, u64)> {
        let seq = seq.to_string();
        self.read(
            BATCH_RECORDS,
            &[
                ("param_gateway", gateway.as_str()),
                ("param_stream", stream.as_str()),
                ("param_seq", &seq),
            ],
        )
        .await
        .lines()
        .map(|line| {
            let fields: Vec<&str> = line.split('\t').collect();
            (
                fields[0].to_owned(),
                fields[1].to_owned(),
                fields[2].parse().expect("a row count"),
            )
        })
        .collect()
    }

    /// Waits until no session holds an advisory lock: an aborted pipeline's lock session has
    /// ended.
    async fn wait_unlocked(&self) {
        wait_for(
            "the aborted pipeline's stream lock released",
            START_LIMIT,
            || async {
                let mut admin = self.product.superuser().await;
                let held: i64 = sqlx::query_scalar(ADVISORY_LOCKS)
                    .fetch_one(&mut admin)
                    .await
                    .expect("read pg_locks");
                (held == 0).then_some(())
            },
        )
        .await;
    }
}

/// A new gateway ID, a stream and two devices.
fn new_gateway(name: &str) -> (GatewayId, StreamId, [uuid::Uuid; 2]) {
    (
        GatewayId::new(format!("gw-{name}-{}", uuid::Uuid::new_v4().simple()))
            .expect("a gateway ID"),
        StreamId::new(format!("s-{name}")).expect("a stream ID"),
        [uuid::Uuid::new_v4(), uuid::Uuid::new_v4()],
    )
}

/// Readings per batch in this test: 2 devices, 2 points, 5 samples.
const RECORDS: u32 = 2 * 2 * 5;

fn payload(stream: &StreamId, seq: u64, devices: &[uuid::Uuid]) -> Vec<u8> {
    batch(stream, seq, devices, 5, now_millis())
        .encode()
        .expect("encode the batch")
}

/// `payload` decoded as plain CBOR, changed by `change`, and encoded again: fields the protocol
/// does not define, or a broken field.
fn edited(
    payload: &[u8],
    change: impl FnOnce(&mut Vec<(cbor4ii::core::Value, cbor4ii::core::Value)>),
) -> Vec<u8> {
    use cbor4ii::core::Value;
    let mut value: Value = cbor4ii::serde::from_slice(payload).expect("the batch is CBOR");
    let Value::Map(entries) = &mut value else {
        panic!("a batch is a map");
    };
    change(entries);
    cbor4ii::serde::to_vec(Vec::new(), &value).expect("encode the edited batch")
}

#[tokio::test(flavor = "multi_thread")]
#[allow(
    clippy::too_many_lines,
    reason = "one scenario on one set of containers, step by step"
)]
async fn telemetry_is_written_once_and_acknowledged_after_it_is_on_disk() {
    let logs = Logs::install();
    let Some(harness) = Harness::start().await else {
        return;
    };
    let mut sender = Gateway::connect(&harness.broker).await;
    let mut gateways: Vec<String> = Vec::new();

    // Background rows of another tenant's devices, for the reads measured in step 7.
    let background = 200_000_u64.to_string();
    let background_tenant = uuid::Uuid::new_v4().to_string();
    harness
        .clickhouse
        .query_with(
            &harness.clickhouse.admin,
            BACKGROUND,
            &[
                ("param_tenant", &background_tenant),
                ("param_rows", &background),
            ],
        )
        .await
        .unwrap_or_else(|error| panic!("insert the background rows: {}", error.body));

    let service = harness
        .pipeline(
            Subscription::Shared("ingest".to_owned()),
            TestHooks::default(),
        )
        .await;

    // 1. Committed once; the tenant is the binding's, not the payload's.
    let (gateway, stream, devices) = new_gateway("once");
    gateways.push(gateway.as_str().to_owned());
    let tenant = uuid::Uuid::new_v4();
    let binding = bind(&harness.product, &gateway, tenant, 1).await;
    let claimed = uuid::Uuid::new_v4().to_string();
    let with_tenant = edited(&payload(&stream, 1, &devices), |entries| {
        for key in ["tenant", "tenant_id"] {
            entries.push((
                cbor4ii::core::Value::Text(key.to_owned()),
                cbor4ii::core::Value::Text(claimed.clone()),
            ));
        }
    });
    sender.send(&gateway, with_tenant.clone(), false).await;
    let ack = sender.ack(&gateway, &stream, 1).await;
    assert_eq!(ack.outcome, AckOutcome::Committed { records: RECORDS });
    assert_eq!(
        harness.batch_rows(&gateway, &stream, 1).await,
        u64::from(RECORDS)
    );
    assert_eq!(
        harness.batch_tenants(&gateway, &stream, 1).await,
        vec![tenant.to_string()],
        "the rows carry the binding's tenant, not the payload's {claimed}"
    );
    assert_eq!(
        harness.batch_records(&gateway, &stream, 1).await,
        vec![(tenant.to_string(), binding.to_string(), u64::from(RECORDS))]
    );
    println!(
        "1. committed: {:?}, {RECORDS} rows, tenant = binding's {tenant} (payload claimed {claimed})",
        ack.outcome
    );
    sender.send(&gateway, with_tenant, false).await;
    let ack = sender.ack(&gateway, &stream, 1).await;
    assert_eq!(ack.outcome, AckOutcome::Duplicate { records: RECORDS });
    assert_eq!(
        harness.batch_rows(&gateway, &stream, 1).await,
        u64::from(RECORDS)
    );
    assert_eq!(harness.batch_records(&gateway, &stream, 1).await.len(), 1);
    println!(
        "1. sent again: {:?}, still {RECORDS} rows, one record",
        ack.outcome
    );

    // 2. Unbound: retry, nothing written. Rejected: a device that is not a UUID, and a payload
    //    whose header reads but whose body does not. No header: no answer, logged.
    let (unbound, unbound_stream, unbound_devices) = new_gateway("unbound");
    sender
        .send(
            &unbound,
            payload(&unbound_stream, 1, &unbound_devices),
            false,
        )
        .await;
    let ack = sender.ack(&unbound, &unbound_stream, 1).await;
    assert_eq!(
        ack.outcome,
        AckOutcome::Retry {
            after_ms: UNBOUND_RETRY_AFTER_MS
        }
    );
    assert_eq!(harness.batch_rows(&unbound, &unbound_stream, 1).await, 0);
    assert!(
        harness
            .batch_records(&unbound, &unbound_stream, 1)
            .await
            .is_empty()
    );
    println!("2. unbound gateway: {:?}, 0 rows, no record", ack.outcome);

    let not_a_uuid = edited(&payload(&stream, 2, &devices), |entries| {
        for (key, value) in entries.iter_mut() {
            if *key == cbor4ii::core::Value::Text("series".to_owned()) {
                let cbor4ii::core::Value::Array(series) = value else {
                    panic!("series is an array");
                };
                let cbor4ii::core::Value::Map(first) = &mut series[0] else {
                    panic!("a series is a map");
                };
                for (field, device) in first.iter_mut() {
                    if *field == cbor4ii::core::Value::Text("device".to_owned()) {
                        *device = cbor4ii::core::Value::Text("meter-1".to_owned());
                    }
                }
            }
        }
    });
    sender.send(&gateway, not_a_uuid, false).await;
    let ack = sender.ack(&gateway, &stream, 2).await;
    let AckOutcome::Rejected { code, message } = &ack.outcome else {
        panic!("a device that is not a UUID: {:?}", ack.outcome);
    };
    assert_eq!(*code, IssueCode::InvalidValue, "{message}");
    assert!(message.starts_with("series[0].device"), "{message}");
    assert_eq!(harness.batch_rows(&gateway, &stream, 2).await, 0);
    println!("2. device not a UUID: rejected {code:?} \"{message}\", 0 rows");

    let broken_body = edited(&payload(&stream, 3, &devices), |entries| {
        for (key, value) in entries.iter_mut() {
            if *key == cbor4ii::core::Value::Text("series".to_owned()) {
                *value = cbor4ii::core::Value::Integer(5);
            }
        }
    });
    sender.send(&gateway, broken_body, false).await;
    let ack = sender.ack(&gateway, &stream, 3).await;
    let AckOutcome::Rejected { code, message } = &ack.outcome else {
        panic!("a body that does not decode: {:?}", ack.outcome);
    };
    assert!(!message.is_empty());
    assert_eq!(harness.batch_rows(&gateway, &stream, 3).await, 0);
    println!("2. header readable, body not: rejected {code:?} \"{message}\", 0 rows");

    sender.send(&gateway, vec![0xff, 0x00, 0x13], false).await;
    let line = logs
        .wait_for(&[
            "dropped a telemetry message without a readable batch header",
            gateway.as_str(),
        ])
        .await;
    println!("2. no header: no answer; logged: {line}");

    // 3. ClickHouse refuses the completion record: retry, rows written, no record. Accepted
    //    again: the resent batch is rewritten and committed, its rows there once.
    harness.clickhouse.admin_ok(REVOKE_COMMIT).await;
    let (failing, failing_stream, failing_devices) = new_gateway("failing");
    gateways.push(failing.as_str().to_owned());
    bind(&harness.product, &failing, tenant, 1).await;
    let failing_batch = payload(&failing_stream, 1, &failing_devices);
    sender.send(&failing, failing_batch.clone(), false).await;
    let ack = sender.ack(&failing, &failing_stream, 1).await;
    assert_eq!(
        ack.outcome,
        AckOutcome::Retry {
            after_ms: RETRY_AFTER_MS
        }
    );
    assert_eq!(
        harness.batch_rows(&failing, &failing_stream, 1).await,
        u64::from(RECORDS),
        "the rows landed before the completion record failed"
    );
    assert!(
        harness
            .batch_records(&failing, &failing_stream, 1)
            .await
            .is_empty()
    );
    let failed = logs
        .wait_for(&[
            "writing a telemetry batch failed",
            failing.as_str(),
            "commit",
        ])
        .await;
    println!(
        "3. completion table refuses: {:?}, {RECORDS} rows, no record; logged: {failed}",
        ack.outcome
    );

    // The same, and the gateway is bound to another tenant before the batch is sent again.
    let (rebound, rebound_stream, rebound_devices) = new_gateway("rebound");
    gateways.push(rebound.as_str().to_owned());
    let first_tenant = uuid::Uuid::new_v4();
    bind(&harness.product, &rebound, first_tenant, 1).await;
    let rebound_batch = payload(&rebound_stream, 1, &rebound_devices);
    sender.send(&rebound, rebound_batch.clone(), false).await;
    let ack = sender.ack(&rebound, &rebound_stream, 1).await;
    assert!(
        matches!(ack.outcome, AckOutcome::Retry { .. }),
        "{:?}",
        ack.outcome
    );
    assert_eq!(
        harness.batch_tenants(&rebound, &rebound_stream, 1).await,
        vec![first_tenant.to_string()]
    );
    let second_tenant = uuid::Uuid::new_v4();
    let second_binding = bind(&harness.product, &rebound, second_tenant, 2).await;

    harness.clickhouse.admin_ok(GRANT_COMMIT).await;
    sender.send(&failing, failing_batch, false).await;
    let ack = sender.ack(&failing, &failing_stream, 1).await;
    assert_eq!(ack.outcome, AckOutcome::Committed { records: RECORDS });
    assert_eq!(
        harness.batch_rows(&failing, &failing_stream, 1).await,
        u64::from(RECORDS),
        "the first attempt's rows were deleted before the batch was written again"
    );
    assert_eq!(
        harness
            .batch_records(&failing, &failing_stream, 1)
            .await
            .len(),
        1
    );
    println!(
        "3. accepted again, resent: {:?}, {RECORDS} rows, one record",
        ack.outcome
    );

    sender.send(&rebound, rebound_batch, false).await;
    let ack = sender.ack(&rebound, &rebound_stream, 1).await;
    assert_eq!(ack.outcome, AckOutcome::Committed { records: RECORDS });
    assert_eq!(
        harness.batch_rows(&rebound, &rebound_stream, 1).await,
        u64::from(RECORDS)
    );
    assert_eq!(
        harness.batch_tenants(&rebound, &rebound_stream, 1).await,
        vec![second_tenant.to_string()],
        "the first tenant's rows of the batch are gone"
    );
    assert_eq!(
        harness.batch_records(&rebound, &rebound_stream, 1).await,
        vec![(
            second_tenant.to_string(),
            second_binding.to_string(),
            u64::from(RECORDS)
        )]
    );
    let moved = logs
        .wait_for(&[
            "earlier attempt wrote this batch for another tenant",
            rebound.as_str(),
        ])
        .await;
    println!(
        "3. rebound between attempts: {:?}, {RECORDS} rows, all under the new tenant; logged: \
         {moved}",
        ack.outcome
    );

    // 4. A retained batch. Sent live, the broker clears the flag (Retain As Published is off)
    //    and it is committed as any batch; the broker keeps it and replays it, with the flag,
    //    to a new subscription without a share group, and that pipeline drops it and logs it.
    let (retained, retained_stream, retained_devices) = new_gateway("retained");
    gateways.push(retained.as_str().to_owned());
    bind(&harness.product, &retained, tenant, 1).await;
    sender
        .send(
            &retained,
            payload(&retained_stream, 1, &retained_devices),
            true,
        )
        .await;
    let ack = sender.ack(&retained, &retained_stream, 1).await;
    assert_eq!(ack.outcome, AckOutcome::Committed { records: RECORDS });
    let direct = harness
        .pipeline(Subscription::Direct, TestHooks::default())
        .await;
    let dropped = logs
        .wait_for(&["dropped a retained telemetry message", retained.as_str()])
        .await;
    let answers = sender
        .acks_within(&retained, &retained_stream, 1, ABSENCE_WINDOW)
        .await;
    assert!(answers.is_empty(), "the replay was answered: {answers:?}");
    assert_eq!(
        harness.batch_rows(&retained, &retained_stream, 1).await,
        u64::from(RECORDS)
    );
    direct.stop().await;
    println!(
        "4. retained batch replayed with the flag: no answer, rows unchanged; logged: {dropped}"
    );

    // 5. and 6. need the pipelines of their own: a second subscriber of the shared group would
    //    take some of the batches.
    service.stop().await;

    // 5. The pipeline ends between the rows and the completion record; a new one rewrites the
    //    resent batch with an identical block and answers only after the record is on disk.
    let (crashed, crashed_stream, crashed_devices) = new_gateway("crash");
    gateways.push(crashed.as_str().to_owned());
    bind(&harness.product, &crashed, tenant, 1).await;
    let received_at = now_millis();
    let crashed_batch = payload(&crashed_stream, 1, &crashed_devices);
    let (at_rows, mut rows_reached, _never) = hold(Point::RowsWritten, 1);
    let first = harness
        .pipeline(
            Subscription::Shared("crash-first".to_owned()),
            TestHooks {
                holds: vec![at_rows],
                received_at: Some(received_at),
            },
        )
        .await;
    sender.send(&crashed, crashed_batch.clone(), false).await;
    reached(&mut rows_reached, "the first pipeline at the rows").await;
    assert_eq!(
        harness.batch_rows(&crashed, &crashed_stream, 1).await,
        u64::from(RECORDS)
    );
    assert!(
        harness
            .batch_records(&crashed, &crashed_stream, 1)
            .await
            .is_empty()
    );
    first.abort().await;
    harness.wait_unlocked().await;
    let answers = sender
        .acks_within(&crashed, &crashed_stream, 1, ABSENCE_WINDOW)
        .await;
    assert!(
        answers.is_empty(),
        "the aborted pipeline answered: {answers:?}"
    );
    println!("5. ended after the rows: {RECORDS} rows, no record, no answer");

    let (at_record, mut record_reached, release) = hold(Point::BeforeCommitRecord, 1);
    let (at_delete, mut delete_reached, delete_release) = hold(Point::Deleted, 1);
    delete_release.add_permits(1);
    let second = harness
        .pipeline(
            Subscription::Shared("crash-second".to_owned()),
            TestHooks {
                holds: vec![at_delete, at_record],
                received_at: Some(received_at),
            },
        )
        .await;
    sender.send(&crashed, crashed_batch, false).await;
    reached(&mut delete_reached, "the second pipeline's DELETE").await;
    reached(&mut record_reached, "the second pipeline before the record").await;
    assert_eq!(
        harness.batch_rows(&crashed, &crashed_stream, 1).await,
        u64::from(RECORDS),
        "the identical block landed again after the DELETE"
    );
    assert!(
        harness
            .batch_records(&crashed, &crashed_stream, 1)
            .await
            .is_empty()
    );
    let early = sender
        .acks_within(&crashed, &crashed_stream, 1, ABSENCE_WINDOW)
        .await;
    assert!(
        early.is_empty(),
        "answered before the completion record: {early:?}"
    );
    release.add_permits(1);
    let ack = sender.ack(&crashed, &crashed_stream, 1).await;
    assert_eq!(ack.outcome, AckOutcome::Committed { records: RECORDS });
    assert_eq!(
        harness
            .batch_records(&crashed, &crashed_stream, 1)
            .await
            .len(),
        1,
        "the record was on disk when `committed` arrived"
    );
    assert_eq!(
        harness.batch_rows(&crashed, &crashed_stream, 1).await,
        u64::from(RECORDS)
    );
    second.stop().await;
    println!(
        "5. resent to a new pipeline: deleted, identical block written again ({RECORDS} rows), \
         no answer while held before the record, then {:?} with the record on disk",
        ack.outcome
    );

    // 6. Two replicas, each in its own share group, so both get every batch.
    let (shared, shared_stream, shared_devices) = new_gateway("replicas");
    gateways.push(shared.as_str().to_owned());
    bind(&harness.product, &shared, tenant, 1).await;
    let (at_lock, mut lock_reached, lock_release) = hold(Point::Locked, 1);
    let holder = harness
        .pipeline(
            Subscription::Shared("replica-one".to_owned()),
            TestHooks {
                holds: vec![at_lock],
                received_at: None,
            },
        )
        .await;
    let shared_batch = payload(&shared_stream, 1, &shared_devices);
    sender.send(&shared, shared_batch.clone(), false).await;
    reached(
        &mut lock_reached,
        "the first replica holding the stream's lock",
    )
    .await;
    let other = harness
        .pipeline(
            Subscription::Shared("replica-two".to_owned()),
            TestHooks::default(),
        )
        .await;
    // The gateway's resend reaches both: the holder's second handler and the other replica
    // find the lock taken.
    sender.send(&shared, shared_batch.clone(), false).await;
    for replica in ["one", "two"] {
        let ack = sender.ack(&shared, &shared_stream, 1).await;
        assert_eq!(
            ack.outcome,
            AckOutcome::Retry {
                after_ms: BUSY_RETRY_AFTER_MS
            },
            "replica {replica} while the lock is held"
        );
    }
    lock_release.add_permits(1);
    let ack = sender.ack(&shared, &shared_stream, 1).await;
    assert_eq!(ack.outcome, AckOutcome::Committed { records: RECORDS });
    println!("6. lock held by one replica: both resends answered retry, then the holder committed");
    // Resent once more: the replica that gets the lock answers duplicate; the other one answers
    // duplicate too, or retry when it asked while the first still held the lock.
    sender.send(&shared, shared_batch, false).await;
    let answers = [
        sender.ack(&shared, &shared_stream, 1).await.outcome,
        sender.ack(&shared, &shared_stream, 1).await.outcome,
    ];
    assert!(
        answers.contains(&AckOutcome::Duplicate { records: RECORDS })
            && answers.iter().all(|outcome| matches!(
                outcome,
                AckOutcome::Duplicate { records: RECORDS }
                    | AckOutcome::Retry {
                        after_ms: BUSY_RETRY_AFTER_MS
                    }
            )),
        "batch 1 resent to both replicas: {answers:?}"
    );
    assert_eq!(
        harness.batch_rows(&shared, &shared_stream, 1).await,
        u64::from(RECORDS),
        "batch 1 after the resends"
    );
    println!("6. batch 1 resent to both replicas: {answers:?}, {RECORDS} rows");
    for seq in 2..=8 {
        sender
            .send(
                &shared,
                payload(&shared_stream, seq, &shared_devices),
                false,
            )
            .await;
        let answers = [
            sender.ack(&shared, &shared_stream, seq).await.outcome,
            sender.ack(&shared, &shared_stream, seq).await.outcome,
        ];
        let committed = answers
            .iter()
            .filter(|outcome| **outcome == AckOutcome::Committed { records: RECORDS })
            .count();
        assert_eq!(committed, 1, "batch {seq}: {answers:?}");
        assert!(
            answers.iter().all(|outcome| matches!(
                outcome,
                AckOutcome::Committed { records: RECORDS }
                    | AckOutcome::Duplicate { records: RECORDS }
                    | AckOutcome::Retry {
                        after_ms: BUSY_RETRY_AFTER_MS
                    }
            )),
            "batch {seq}: {answers:?}"
        );
        assert_eq!(
            harness.batch_rows(&shared, &shared_stream, seq).await,
            u64::from(RECORDS),
            "batch {seq}"
        );
        println!("6. batch {seq} to both replicas: {answers:?}, {RECORDS} rows");
    }
    holder.stop().await;
    other.stop().await;

    // 7. Over everything written.
    let gateways = format!(
        "[{}]",
        gateways
            .iter()
            .map(|gateway| format!("'{gateway}'"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    for (table, statement) in [("telemetry_1m", RAW_VS_1M), ("telemetry_1h", RAW_VS_1H)] {
        let answer = harness
            .read(statement, &[("param_gateways", &gateways)])
            .await;
        let fields: Vec<&str> = answer.trim().split('\t').collect();
        assert_eq!(
            fields[0], "0",
            "{table}: cells that differ from the raw rows"
        );
        assert_ne!(fields[1], "0", "{table}: no cells compared");
        println!(
            "7. {table} against the raw rows: {} cells, {} differ",
            fields[1], fields[0]
        );
    }
    let answer = harness.read(COMMITTED_AFTER_RECEIVED, &[]).await;
    let fields: Vec<&str> = answer.trim().split('\t').collect();
    let smallest: i64 = fields[0].parse().expect("a difference");
    assert!(
        smallest >= 0,
        "a completion record earlier than its rows: {answer}"
    );
    println!(
        "7. min(committed_at) - max(received_at) per batch: smallest {smallest} ms over {} batches",
        fields[1]
    );

    harness.clickhouse.admin_ok(FLUSH_LOGS).await;
    let statements = harness
        .clickhouse
        .query_with(
            &harness.clickhouse.admin,
            STATEMENT_SETTINGS,
            &[("param_user", USER)],
        )
        .await
        .unwrap_or_else(|error| panic!("read the query log: {}", error.body));
    let mut kinds = Vec::new();
    for line in statements.lines() {
        let fields: Vec<&str> = line.split('\t').collect();
        assert_eq!(
            fields[1], fields[2],
            "statements of kind {} without the stream settings",
            fields[0]
        );
        kinds.push(fields[0].to_owned());
        println!(
            "7. statements of kind {}: {}, with the stream settings: {}",
            fields[0], fields[1], fields[2]
        );
    }
    for kind in ["Delete", "Insert", "Select"] {
        assert!(
            kinds.iter().any(|seen| seen == kind),
            "no {kind} statement: {statements}"
        );
    }
    let reads = harness
        .clickhouse
        .query_with(
            &harness.clickhouse.admin,
            LEFTOVER_READS,
            &[("param_user", USER)],
        )
        .await
        .unwrap_or_else(|error| panic!("read the query log: {}", error.body));
    let total = harness.read(TABLE_ROWS, &[]).await;
    for line in reads.lines() {
        let fields: Vec<&str> = line.split('\t').collect();
        let read: u64 = fields[2].parse().expect("read rows");
        let total: u64 = total.trim().parse().expect("a count");
        assert!(
            read < total / 2,
            "{} read {read} of {total} rows",
            fields[0]
        );
        println!(
            "7. {} ({} statements): at most {} rows ({} bytes) read of {total} in the table",
            if fields[0] == "Delete" {
                "DELETE"
            } else {
                "leftover search"
            },
            fields[1],
            fields[2],
            fields[3]
        );
    }
    let comment = harness.clickhouse.admin_ok(TENANT_COMMENT).await;
    assert!(
        comment.contains("binding table") && !comment.contains("certificate"),
        "{comment}"
    );
    println!("7. telemetry.tenant_id comment: {}", comment.trim());

    // No reading value and no password in the log.
    let text = logs.lines().join("\n");
    assert!(!text.contains(&harness.service_password));
    assert!(!text.contains("value_float"));
    let _ = std::io::stdout().flush();
}
