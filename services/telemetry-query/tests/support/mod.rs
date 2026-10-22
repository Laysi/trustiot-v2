//! Helpers for the integration tests: the telemetry-query binary as a real process on the
//! product's Postgres schema (`testkit::product`) and a throwaway ClickHouse with the product's
//! ClickHouse schema, its JSON log, plain HTTP calls to it, and test data in ClickHouse.
#![allow(
    dead_code,
    reason = "each test binary includes this module and uses a different subset of it"
)]

/// The ClickHouse container and its HTTP client, shared with the ingest service's schema tests.
#[path = "../../../ingest/tests/common/mod.rs"]
pub mod clickhouse;

use std::io::{BufRead, BufReader, Read};
use std::net::SocketAddr;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use contract::{ServiceName, TenantId};
use identity::tenants::{TenantName, TenantStore};
use serde_json::Value;
use testkit::identity::TestIssuer;
use testkit::postgres::TEST_PASSWORD;
use testkit::product::{DATABASE, ProductPostgres, product_postgres};

use self::clickhouse::{Account, ClickHouse};

/// The telemetry-query binary, built by Cargo for these tests.
pub const TELEMETRY_QUERY: &str = env!("CARGO_BIN_EXE_telemetry-query");

/// The service's ClickHouse account (`0007_telemetry_query.sql`).
pub const SERVICE_ACCOUNT: &str = "trustiot_telemetry_query";

/// The Postgres roles the tests log in with.
pub const ROLES: [&str; 3] = [
    "identity_app",
    "telemetry_query_app",
    "telemetry_query_platform",
];

/// The longest a started service may take to listen and to become ready. Generous: test hosts
/// are often loaded (CLAUDE.md, "Test time limits").
pub const START_LIMIT: Duration = Duration::from_secs(60);

/// The longest the consumer may take to create a tenant's account once the tenant is committed.
/// It polls every second when idle; the bound is for a loaded host.
pub const EVENT_LIMIT: Duration = Duration::from_secs(60);

/// Sets the service account's password and lets it log in, as the migration Job does with the
/// password it generates into the service's Secret (tools/migrate tests the Job's part).
const SET_SERVICE_PASSWORD: &str = "ALTER USER trustiot_telemetry_query
IDENTIFIED WITH sha256_password BY {password:String} HOST ANY";

/// An account that reads every row of the telemetry table, for the expected results the tests
/// compare the API with. Test-only: the product has no such reader.
const CREATE_REFERENCE: &str = "CREATE USER IF NOT EXISTS trustiot_test_reference
IDENTIFIED WITH sha256_password BY {password:String} HOST ANY";
const GRANT_REFERENCE: &str = "GRANT SELECT ON trustiot.telemetry TO trustiot_test_reference";
const REFERENCE_POLICY: &str = "CREATE ROW POLICY IF NOT EXISTS test_reference_all_rows
ON trustiot.telemetry AS PERMISSIVE FOR SELECT USING 1 TO trustiot_test_reference";

/// Readings of one data point: `n` readings `step` milliseconds apart from `start` (Unix
/// milliseconds), of the type `type`, with values that vary with the reading's number. Written
/// as the admin, as the ingest service writes them.
const INSERT_READINGS: &str = "INSERT INTO trustiot.telemetry (tenant_id, gateway_id, stream_id,
  batch_seq, device_id, point, ts, received_at, value_type, value_float, value_int, value_bool,
  value_string)
SELECT {tenant:UUID}, generateUUIDv4(), generateUUIDv4(), number, {device:UUID}, {point:String},
  fromUnixTimestamp64Milli({start:Int64} + toInt64(number) * {step:Int64}), now64(3),
  {type:String},
  if({type:String} = 'float', round(sin(number) * 100, 3), 0),
  if({type:String} = 'int', toInt64(number) * 7 - 50, 0),
  {type:String} = 'bool' AND number % 3 = 0,
  if({type:String} = 'string', concat({marker:String}, '-', toString(number)), '')
FROM numbers({n:UInt64})";

/// Postgres with the product's schema, ClickHouse with the product's ClickHouse schema, and the
/// service's ClickHouse password.
pub struct Stack {
    pub product: Arc<ProductPostgres>,
    pub clickhouse: ClickHouse,
    pub service_password: String,
    pub reference: Account,
}

/// A random password of 64 hex digits, made at run time.
pub fn random_password() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

impl Stack {
    /// Starts both servers and applies both schemas; `None` (the test skips) without podman or
    /// docker, unless `CI` is set.
    pub async fn start() -> Option<Self> {
        let product = product_postgres(&ROLES).await?;
        let clickhouse = ClickHouse::start().await?;
        for migration in migrate::registry::PRODUCT.clickhouse {
            let statements = migrate::clickhouse::split_statements(migration.sql)
                .unwrap_or_else(|error| panic!("{}: {error}", migration.name));
            for statement in statements {
                clickhouse.admin(&statement).await.unwrap_or_else(|error| {
                    panic!("{}: {statement}\n{}", migration.name, error.body)
                });
            }
        }
        let service_password = random_password();
        clickhouse
            .query_with(
                &clickhouse.admin,
                SET_SERVICE_PASSWORD,
                &[("param_password", &service_password)],
            )
            .await
            .unwrap_or_else(|error| panic!("set the service's password: {}", error.body));
        let reference = Account {
            user: "trustiot_test_reference".to_owned(),
            password: random_password(),
        };
        clickhouse
            .query_with(
                &clickhouse.admin,
                CREATE_REFERENCE,
                &[("param_password", &reference.password)],
            )
            .await
            .unwrap_or_else(|error| panic!("create the reference account: {}", error.body));
        for statement in [GRANT_REFERENCE, REFERENCE_POLICY] {
            clickhouse.admin_ok(statement).await;
        }
        Some(Self {
            product,
            clickhouse,
            service_password,
            reference,
        })
    }

    /// The service's view of tenant accounts, to compute a tenant account's name and password.
    pub fn accounts(&self) -> telemetry_query::accounts::Accounts {
        let client = telemetry_query::clickhouse::ClickHouse::new(&self.clickhouse.url, None)
            .expect("a ClickHouse client");
        telemetry_query::accounts::Accounts::new(client, SERVICE_ACCOUNT, &self.service_password)
    }

    /// `tenant`'s ClickHouse account, as the service derives it.
    pub fn tenant_account(&self, tenant: TenantId) -> Account {
        let account = self.accounts().account(tenant);
        let credentials = account.credentials();
        Account {
            user: credentials.user.to_owned(),
            password: credentials.password.to_owned(),
        }
    }

    /// Creates a tenant through the identity service's store, as `POST
    /// /api/identity/v1/tenants` does; the service gets `identity.tenant_created` from its outbox.
    pub async fn create_tenant(&self, issuer: &TestIssuer, name: &str) -> TenantId {
        let store =
            TenantStore::connect_lazy(self.product.postgres.options("identity_app", DATABASE))
                .expect("verify-full options");
        let name = TenantName::parse(name).expect("a valid name");
        let mut claims = TestIssuer::claims(ServiceName::Identity, None);
        claims.platform_admin = true;
        store
            .create(&issuer.user(ServiceName::Identity, &claims), &name)
            .await
            .expect("create the tenant")
            .tenant_id
    }

    /// Writes readings (see [`INSERT_READINGS`]); `marker` starts every text reading.
    pub async fn insert(&self, readings: &Readings<'_>) {
        let (tenant, device) = (readings.tenant.to_string(), readings.device.to_string());
        let (start, step, n) = (
            readings.start_ms.to_string(),
            readings.step_ms.to_string(),
            readings.count.to_string(),
        );
        self.clickhouse
            .query_with(
                &self.clickhouse.admin,
                INSERT_READINGS,
                &[
                    ("param_tenant", &tenant),
                    ("param_device", &device),
                    ("param_point", readings.point),
                    ("param_type", readings.kind),
                    ("param_start", &start),
                    ("param_step", &step),
                    ("param_n", &n),
                    ("param_marker", readings.marker),
                ],
            )
            .await
            .unwrap_or_else(|error| panic!("insert readings: {}", error.body));
    }

    /// Runs `statement` as the reference account, which reads every row, with query parameters
    /// `params` (names without `param_`).
    pub async fn reference(&self, statement: &str, params: &[(&str, &str)]) -> String {
        let named: Vec<(String, &str)> = params
            .iter()
            .map(|(name, value)| (format!("param_{name}"), *value))
            .collect();
        let pairs: Vec<(&str, &str)> = named
            .iter()
            .map(|(name, value)| (name.as_str(), *value))
            .collect();
        self.clickhouse
            .query_with(&self.reference, statement, &pairs)
            .await
            .unwrap_or_else(|error| panic!("reference query failed: {statement}\n{}", error.body))
    }
}

/// Readings to write with [`Stack::insert`].
pub struct Readings<'a> {
    pub tenant: TenantId,
    pub device: uuid::Uuid,
    pub point: &'a str,
    /// `float`, `int`, `bool` or `string`.
    pub kind: &'a str,
    pub start_ms: i64,
    pub step_ms: i64,
    pub count: u64,
    pub marker: &'a str,
}

/// Writes `jwks` to a file of its own under Cargo's temporary directory for these tests and
/// returns its path.
fn keys_file(jwks: &str) -> std::path::PathBuf {
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let name = format!(
        "telemetry-query-jwks-{}-{}.json",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let path = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    std::fs::write(&path, jwks).expect("write the keys file");
    path
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

/// Where the service finds ClickHouse and which account it uses.
pub struct ClickHouseTarget<'a> {
    pub url: &'a str,
    pub password: &'a str,
}

/// The telemetry-query service running as a child process.
pub struct Service {
    child: Child,
    pub stdout: Output,
    pub stderr: Output,
    /// Where the API listens, as a base URL.
    pub base_url: String,
    health: SocketAddr,
}

impl Service {
    /// Starts the service on free local ports against `product` and `clickhouse`, trusting the
    /// internal identity keys `jwks`, with the roles the chart gives it, and waits until it is
    /// ready. The environment is otherwise empty, so the developer's own variables cannot leak
    /// in.
    pub async fn start(
        product: &ProductPostgres,
        clickhouse: &ClickHouseTarget<'_>,
        jwks: &str,
    ) -> Self {
        let keys_file = keys_file(jwks);
        let mut command = Command::new(TELEMETRY_QUERY);
        command
            .env_clear()
            .env("TRUSTIOT__HTTP__LISTEN_ADDR", "127.0.0.1:0")
            .env("TRUSTIOT__HTTP__HEALTH_LISTEN_ADDR", "127.0.0.1:0")
            .env("TRUSTIOT__SHUTDOWN__DRAIN_DELAY", "100ms")
            .env("TRUSTIOT__SHUTDOWN__DRAIN_TIMEOUT", "5s")
            .env("TRUSTIOT_INTERNAL_IDENTITY_KEYS_FILE", &keys_file)
            .env("TRUSTIOT__DATABASE__USERNAME", "telemetry_query_app")
            .env("TRUSTIOT__DATABASE__PASSWORD", TEST_PASSWORD)
            .env(
                "TRUSTIOT__DATABASE__PLATFORM_USERNAME",
                "telemetry_query_platform",
            )
            .env("TRUSTIOT__DATABASE__PLATFORM_PASSWORD", TEST_PASSWORD)
            .env("TRUSTIOT_CLICKHOUSE_URL", clickhouse.url)
            .env("TRUSTIOT__CLICKHOUSE__USERNAME", SERVICE_ACCOUNT)
            .env("TRUSTIOT__CLICKHOUSE__PASSWORD", clickhouse.password)
            .envs(product.platform_env())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().expect("start the telemetry-query service");
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
                "the telemetry-query service did not log `listening` within {START_LIMIT:?}\n\
                 stdout:\n{}\nstderr:\n{}",
                stdout.text(),
                stderr.text()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        let address = |field: &str| -> SocketAddr {
            listening["fields"][field]
                .as_str()
                .and_then(|address| address.parse().ok())
                .unwrap_or_else(|| panic!("no {field} in {listening}"))
        };
        let service = Self {
            child,
            base_url: format!("http://{}", address("address")),
            health: address("health_address"),
            stdout,
            stderr,
        };
        service.wait_ready().await;
        service
    }

    /// Waits until `/readyz` answers 200.
    pub async fn wait_ready(&self) {
        let url = format!("http://{}/readyz", self.health);
        let deadline = Instant::now() + START_LIMIT;
        loop {
            if let Ok(response) = reqwest::get(&url).await
                && response.status() == 200
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "/readyz of the telemetry-query service did not answer 200 within \
                 {START_LIMIT:?}\nstdout:\n{}",
                self.stdout.text()
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// `GET path` with an `x-internal-identity` token (none: without the header); the status
    /// and the body.
    pub async fn get(&self, path: &str, token: Option<&str>) -> (u16, String) {
        let mut request = reqwest::Client::new()
            .get(format!("{}{path}", self.base_url))
            // Above the 45 s the service waits for ClickHouse.
            .timeout(Duration::from_secs(90));
        if let Some(token) = token {
            request = request.header("x-internal-identity", token);
        }
        let response = request
            .send()
            .await
            .expect("call the telemetry-query service");
        let status = response.status().as_u16();
        (status, response.text().await.expect("read the body"))
    }

    /// `GET path` that must answer 200; the body as JSON.
    pub async fn get_ok(&self, path: &str, token: &str) -> Value {
        let (status, body) = self.get(path, Some(token)).await;
        assert_eq!(status, 200, "GET {path}: {body}");
        serde_json::from_str(&body).unwrap_or_else(|error| panic!("GET {path}: {error}: {body}"))
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

/// Waits until the API answers 200 for `tenant`, which needs its account: the consumer created
/// it from `identity.tenant_created`.
pub async fn wait_account_ready(service: &Service, token: &str, tenant: TenantId) {
    let path = format!("/api/telemetry/v1/devices/{}/latest", uuid::Uuid::nil());
    wait_for(
        &format!("the account of tenant {tenant}, so that GET {path} answers 200"),
        EVENT_LIMIT,
        || async {
            let (status, _) = service.get(&path, Some(token)).await;
            (status == 200).then_some(())
        },
    )
    .await;
}
