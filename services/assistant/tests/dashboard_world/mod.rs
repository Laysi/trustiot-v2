//! The world of the dashboard tests (Laysi/trustiot-v2#150): the product's schema with the
//! service role `assistant_app` (`testkit::product`), users with a chosen tenant and role,
//! conversations, drafts written as the `dashboard` tool writes them (through the store), one
//! stand-in server for every peer a dataset run calls, the public API served in the test
//! process, the assistant binary as the chart starts it, a question's delegation, and the
//! `dashboard` tool as the service builds it.
//!
//! The stand-in peers share one loopback socket:
//!
//! - the identity service's token exchange (`/internal/token-exchange`): it verifies the
//!   request's token for the assistant and exchanges it as the identity service does, for the
//!   same user, tenant and role, addressed to the asked audience, with `act` naming the
//!   assistant and no later expiry than the request's token;
//! - telemetry-query's SQL, statistics and usage routes: they verify the token for
//!   telemetry-query, record the call with the token's claims, and answer with
//!   telemetry-query's own response types, or as the test scripts it;
//! - anything else, which only the AI gateway or a service no dashboard calls would get, is
//!   recorded as unexpected and refused.
//!
//! Every token is signed when it is sent, valid for 60 seconds as the ingress signs them, so a
//! slow test never sends an expired one.

#![allow(
    dead_code,
    reason = "each test binary uses a different part of the world"
)]

use std::future::Future;
use std::io::{BufRead, BufReader, Read};
use std::net::SocketAddr;
use std::pin::Pin;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use assistant::api::held_body::HELD_BODY_TIMEOUT;
use assistant::api::{self, dashboards::Dashboards};
use assistant::callee::Callee;
use assistant::dashboards::caps::{Limited, RunCaps};
use assistant::dashboards::checker::Checker;
use assistant::dashboards::limits::VersionBudget;
use assistant::dashboards::runner::DatasetRunner;
use assistant::dashboards::store::{self, Author, NewVersion, StoreError};
use assistant::delegation::IDENTITY_TIMEOUT;
use assistant::history::{Conversation, Preamble, Principal, SystemPrompt};
use assistant::model::anthropic::AnthropicGateway;
use assistant::model::{CacheTtl, Effort};
use assistant::tools::dashboard::{DashboardTool, WriteBounds};
use assistant::tools::{Delegation, DelegationError, ToolLimits, ToolRegistry};
use assistant::{Agent, AgentSettings};
use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use chrono::{DateTime, TimeDelta, Utc};
use contract::api::dashboard::RuntimeVersion;
use contract::api::dashboard::public::{DashboardKind, VersionCreated};
use contract::api::dashboard::spec::Spec;
use contract::api::identity::ExchangeToken;
use contract::api::telemetry_query::{QueryableFrom, RunSql, SqlColumn, SqlResult};
use contract::client::{Endpoint as _, InternalClient};
use contract::{
    Actor, DashboardId, ErrorBody, ErrorCode, INTERNAL_IDENTITY_HEADER, InternalClaims,
    InternalToken, ServiceName, TenantGrant, TenantId, TenantRole, service,
};
use serde_json::{Value, json};
use service_kit::identity::{OnBehalf, ServiceIdentity, UserIdentity};
use service_kit::tenant::{BackgroundTenantDb, PlatformDb, TenantDb, TenantTransaction};
use sqlx::PgConnection;
use sqlx::postgres::PgPoolOptions;
use telemetry_query::history::Reading;
use telemetry_query::semantic::range::Resolution;
use telemetry_query::semantic::{
    CumulativeCheck, DataStatus, QueryRange, SeriesQuery, SeriesStatistics, SeriesUsage,
    Statistics, StatisticsRow, Usage, UsageRow, UsageStatus, Verdict,
};
use testkit::identity::TestIssuer;
use testkit::postgres::TEST_PASSWORD;
use testkit::product::{DATABASE, ProductPostgres, product_postgres};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use uuid::Uuid;

/// The longest a test waits for something it caused: a bound only, far above what any of it
/// takes, so a loaded machine does not fail the test (CLAUDE.md, 「Test time limits」).
pub const WAIT: Duration = Duration::from_secs(120);

/// The device of the semantic datasets.
pub const DEVICE: &str = "0192f6e0-7a3c-7d4e-8f00-0123456789ab";

/// The paths telemetry-query's stand-in answers.
pub const SQL_PATH: &str = RunSql::ROUTE;
pub const STATISTICS_PATH: &str = "/api/telemetry/v1/statistics";
pub const USAGE_PATH: &str = "/api/telemetry/v1/usage";

/// The datasets of [`spec`]: a SQL one, a statistics one and a usage one.
pub const DATASETS: [&str; 3] = ["daily", "stats", "usage"];

pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

fn unix_now() -> i64 {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("after the epoch");
    i64::try_from(elapsed.as_secs()).expect("seconds fit")
}

/// A spec with the three [`DATASETS`], each with `max_rows` 2000, 100 and 100.
pub fn spec() -> Spec {
    serde_json::from_value(json!({
        "datasets": [
            {
                "name": "daily",
                "title": "Daily energy",
                "description": "Energy per day, summed from the meters' readings.",
                "max_rows": 2000,
                "source": {
                    "kind": "sql",
                    "sql": "SELECT toDate(time) AS day, sum(value) AS kwh FROM trustiot.tenant_telemetry GROUP BY day ORDER BY day"
                }
            },
            {
                "name": "stats",
                "title": "Voltage",
                "description": "The voltage's statistics over the last seven days.",
                "max_rows": 100,
                "source": {
                    "kind": "telemetry_statistics",
                    "request": {
                        "series": [{"device_id": DEVICE, "point": "voltage"}],
                        "tz": "Asia/Taipei",
                        "period": "last_7_days",
                        "interval": "total"
                    }
                }
            },
            {
                "name": "usage",
                "title": "Usage",
                "description": "Energy used per day over the last seven days.",
                "max_rows": 100,
                "source": {
                    "kind": "telemetry_usage",
                    "request": {
                        "series": [{"device_id": DEVICE, "point": "energy_kwh"}],
                        "tz": "Asia/Taipei",
                        "period": "last_7_days",
                        "interval": "1d"
                    }
                }
            }
        ]
    }))
    .expect("the test spec parses")
}

/// A version's content as the `dashboard` tool writes it.
#[derive(Clone, Debug)]
pub struct Content {
    pub title: String,
    pub description: Option<String>,
    pub spec: Spec,
    pub page: String,
}

impl Content {
    /// [`spec`] with a page that names `title`.
    pub fn new(title: &str) -> Self {
        Self {
            title: title.to_owned(),
            description: Some(format!("{title}, as the test wrote it.")),
            spec: spec(),
            page: format!("<main><h1>{title}</h1><div id=\"chart\"></div></main>"),
        }
    }

    pub fn version(&self) -> NewVersion<'_> {
        NewVersion {
            runtime: RuntimeVersion::CURRENT,
            title: &self.title,
            description: self.description.as_deref(),
            spec: &self.spec,
            page: &self.page,
        }
    }
}

/// A user of one tenant with one role: the claims the ingress signs for the assistant.
#[derive(Clone, Debug)]
pub struct Person {
    pub claims: InternalClaims,
}

impl Person {
    pub fn tenant(&self) -> TenantId {
        self.claims.tenant.expect("a person has a tenant").id
    }

    pub fn role(&self) -> TenantRole {
        self.claims.tenant.expect("a person has a tenant").role
    }

    pub fn principal(&self) -> Principal {
        Principal {
            tenant_id: self.tenant(),
            user_id: self.claims.sub,
        }
    }

    /// The author of a tool's write: this user's question.
    pub fn author(&self) -> Author {
        Author {
            principal: self.principal(),
            run_id: Uuid::now_v7(),
        }
    }

    /// The same user with another role in the same tenant.
    pub fn with_role(&self, role: TenantRole) -> Self {
        let mut claims = self.claims.clone();
        claims.tenant = Some(TenantGrant {
            id: self.tenant(),
            role,
        });
        Self { claims }
    }
}

/// A question's delegation: tokens of `claims`' user and tenant for whichever service a tool
/// calls, exchanged by the assistant (`act`), until it is revoked.
pub struct Question {
    pub issuer: Arc<TestIssuer>,
    pub claims: InternalClaims,
    pub deadline: Option<tokio::time::Instant>,
    pub revoked: AtomicBool,
}

impl Question {
    /// A question of `person` without a deadline.
    pub fn of(world: &World, person: &Person) -> Self {
        Self {
            issuer: Arc::clone(&world.issuer),
            claims: person.claims.clone(),
            deadline: None,
            revoked: AtomicBool::new(false),
        }
    }
}

impl Delegation for Question {
    fn token_for(
        &self,
        audience: ServiceName,
    ) -> assistant::model::BoxFuture<'_, Result<InternalToken, DelegationError>> {
        if self.revoked.load(Ordering::SeqCst) {
            return Box::pin(async { Err(DelegationError::Revoked) });
        }
        let mut claims = self.claims.clone();
        let now = unix_now();
        claims.aud = audience;
        claims.iat = now;
        claims.exp = now + testkit::identity::TOKEN_LIFETIME_SECS;
        claims.act = Some(Actor {
            sub: ServiceName::Assistant,
        });
        let token = InternalToken::new(self.issuer.token(&claims))
            .map_err(|_| DelegationError::Unavailable);
        Box::pin(async move { token })
    }

    fn deadline(&self) -> Option<tokio::time::Instant> {
        self.deadline
    }
}

/// The `dashboard` tool as `assistant::build` makes it, running datasets against telemetry-query
/// at `telemetry_url` and checking with the service at `check_url`, with `check_timeout` per
/// check (`CHECK_CALL_TIMEOUT` in the service) and `datasets_timeout` until a write's datasets
/// have run (`DATASETS_TIMEOUT`).
pub fn dashboard_tool(
    world: &World,
    telemetry_url: &str,
    check_url: &str,
    check_timeout: Duration,
    datasets_timeout: Duration,
) -> Arc<DashboardTool> {
    let checker = Checker::new(check_url, check_timeout).expect("a checker");
    dashboard_tool_with(
        world,
        telemetry_url,
        checker,
        WriteBounds {
            datasets_timeout,
            ..WriteBounds::default()
        },
    )
}

/// [`dashboard_tool`] with `checker` and `bounds` as the test makes them.
pub fn dashboard_tool_with(
    world: &World,
    telemetry_url: &str,
    checker: Checker,
    bounds: WriteBounds,
) -> Arc<DashboardTool> {
    let telemetry = Callee::new(
        ServiceName::TelemetryQuery,
        telemetry_url,
        Duration::from_secs(60),
    )
    .expect("a client for telemetry-query");
    let runner = Arc::new(DatasetRunner::new(Some(telemetry), RunCaps::default()));
    dashboard_tool_on(world, runner, checker, bounds)
}

/// [`dashboard_tool_with`] running datasets with `runner`, whose memory the test looks at.
pub fn dashboard_tool_on(
    world: &World,
    runner: Arc<DatasetRunner>,
    checker: Checker,
    bounds: WriteBounds,
) -> Arc<DashboardTool> {
    dashboard_tool_on_pool(world, runner, checker, bounds, 4).0
}

/// [`dashboard_tool_on`] with a pool of `max_connections`, which the test may take connections
/// from too: the tool and the pool.
pub fn dashboard_tool_on_pool(
    world: &World,
    runner: Arc<DatasetRunner>,
    checker: Checker,
    bounds: WriteBounds,
    max_connections: u32,
) -> (Arc<DashboardTool>, BackgroundTenantDb) {
    let (_, background) = TenantDb::connect_lazy_with_background(
        world.product.postgres.options("assistant_app", DATABASE),
        PgPoolOptions::new().max_connections(max_connections),
        ServiceIdentity::new(ServiceName::Assistant),
    )
    .expect("the service role's pool verifies the server");
    (
        DashboardTool::with_bounds(background.clone(), runner, checker, bounds),
        background,
    )
}

/// The database, the role and the token issuer.
pub struct World {
    pub product: Arc<ProductPostgres>,
    pub db: TenantDb,
    pub issuer: Arc<TestIssuer>,
}

/// The world, or `None` without a container runtime (testkit skips locally, fails in CI).
pub async fn world() -> Option<World> {
    let product = product_postgres(&["assistant_app", "assistant_platform"]).await?;
    let db = TenantDb::connect_lazy(
        product.postgres.options("assistant_app", DATABASE),
        PgPoolOptions::new().max_connections(8),
    )
    .expect("the service role's pool verifies the server");
    Some(World {
        product,
        db,
        issuer: Arc::new(TestIssuer::new()),
    })
}

/// The budget a store test gives publish and restore: every version may be added.
pub fn unlimited(_: DashboardKind) -> Result<(), Limited> {
    Ok(())
}

/// The server process of `tx`'s session, for [`World::wait_until_blocking`].
pub async fn backend_pid(tx: &mut TenantTransaction) -> i32 {
    sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut **tx)
        .await
        .expect("the session's process")
}

/// Ends `tx`: commits it when `result` is `Ok`, rolls it back otherwise.
pub async fn finish<T>(
    tx: TenantTransaction,
    result: Result<T, StoreError>,
) -> Result<T, StoreError> {
    if result.is_ok() {
        tx.commit().await.expect("commit");
    } else {
        tx.rollback().await.expect("roll back");
    }
    result
}

impl World {
    /// A new user of `tenant` with `role`.
    pub fn person(&self, tenant: TenantId, role: TenantRole) -> Person {
        Person {
            claims: TestIssuer::claims(
                ServiceName::Assistant,
                Some(TenantGrant { id: tenant, role }),
            ),
        }
    }

    /// A new user without a tenant.
    pub fn tenantless(&self) -> Person {
        Person {
            claims: TestIssuer::claims(ServiceName::Assistant, None),
        }
    }

    /// The person's token for the assistant, issued now.
    pub fn token(&self, person: &Person) -> String {
        let mut claims = person.claims.clone();
        claims.iat = unix_now();
        claims.exp = claims.iat + testkit::identity::TOKEN_LIFETIME_SECS;
        self.issuer.token(&claims)
    }

    /// The person as a handler of the assistant receives them.
    pub fn user(&self, person: &Person) -> UserIdentity {
        self.issuer
            .verifier(ServiceName::Assistant)
            .verify(&self.token(person))
            .expect("the test token verifies")
    }

    pub async fn superuser(&self) -> PgConnection {
        self.product.superuser().await
    }

    /// A transaction of the person's tenant, as the API opens one.
    pub async fn tx(&self, person: &Person) -> TenantTransaction {
        self.db
            .begin(&self.user(person))
            .await
            .expect("a tenant transaction")
    }

    /// A new conversation of `owner`'s with nothing asked yet, stored as the service stores
    /// one ([`World::store_conversation`]).
    pub async fn conversation(&self, owner: &Person) -> Uuid {
        let preamble = Preamble::new(
            SystemPrompt {
                version: "test".to_owned(),
                text: "You are a test.".to_owned(),
            },
            String::new(),
            Vec::new(),
            Effort::Medium,
            CacheTtl::FiveMinutes,
        );
        let conversation = Conversation::new(owner.principal(), Arc::new(preamble));
        self.store_conversation(owner, &conversation).await;
        conversation.id()
    }

    /// Deletes `owner`'s conversation `id` through the store, as the API's `DELETE` does.
    pub async fn delete_conversation(&self, owner: &Person, id: Uuid) {
        let mut tx = self.tx(owner).await;
        assistant::store::delete(&mut tx, owner.principal(), id)
            .await
            .expect("delete the conversation");
        tx.commit().await.expect("commit");
    }

    /// Restores `owner`'s deleted conversation `id` through the store, as the API's restore does.
    pub async fn restore_conversation(&self, owner: &Person, id: Uuid) {
        let mut tx = self.tx(owner).await;
        assistant::store::restore(&mut tx, owner.principal(), id)
            .await
            .expect("restore the conversation");
        tx.commit().await.expect("commit");
    }

    /// Waits until another session waits for a lock that the session `holder` (its
    /// [`backend_pid`]) holds; `what` names the waiting statement in the failure.
    pub async fn wait_until_blocking(&self, holder: i32, what: &str) {
        let mut admin = self.superuser().await;
        let started = Instant::now();
        loop {
            let blocks: bool = sqlx::query_scalar(
                "SELECT EXISTS (
                    SELECT 1 FROM pg_stat_activity WHERE $1 = ANY (pg_blocking_pids(pid)))",
            )
            .bind(holder)
            .fetch_one(&mut admin)
            .await
            .expect("read the lock waits");
            if blocks {
                return;
            }
            assert!(
                started.elapsed() < WAIT,
                "{what} did not wait for the row lock within {WAIT:?}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Moves the deletion of the deleted conversation `id` back to `deleted_at`, behind the
    /// store's back: the clock trigger refuses any other time, so the superuser skips it.
    pub async fn backdate(&self, id: Uuid, deleted_at: DateTime<Utc>) {
        let mut admin = self.superuser().await;
        let mut tx = sqlx::Connection::begin(&mut admin)
            .await
            .expect("a superuser transaction");
        sqlx::query("SET LOCAL session_replication_role = replica")
            .execute(&mut *tx)
            .await
            .expect("skip the clock trigger");
        let updated = sqlx::query(
            "UPDATE assistant.conversations SET deleted_at = $2
            WHERE conversation_id = $1::uuid AND deleted_at IS NOT NULL",
        )
        .bind(id.to_string())
        .bind(deleted_at)
        .execute(&mut *tx)
        .await
        .expect("move the deletion back")
        .rows_affected();
        assert_eq!(updated, 1, "the deleted conversation to move back exists");
        tx.commit().await.expect("commit");
    }

    /// The purge's platform role (`assistant_platform`), with the pool the service opens for it.
    pub fn platform(&self) -> PlatformDb {
        PlatformDb::connect_lazy(
            self.product
                .postgres
                .options("assistant_platform", DATABASE),
            PgPoolOptions::new().max_connections(assistant::PLATFORM_CONNECTIONS),
        )
        .expect("the platform role's pool verifies the server")
    }

    /// Stores `owner`'s `conversation` (one the agent started) as the service does when its first
    /// question starts: `assistant::store::create` in a transaction of the owner's tenant.
    pub async fn store_conversation(&self, owner: &Person, conversation: &Conversation) {
        let mut tx = self.tx(owner).await;
        assistant::store::create(&mut tx, conversation)
            .await
            .expect("store the conversation");
        tx.commit().await.expect("commit");
    }

    /// A new draft in `owner`'s `conversation` with `content` as its version 1.
    pub async fn create_draft(
        &self,
        owner: &Person,
        conversation: Uuid,
        content: &Content,
    ) -> VersionCreated {
        let mut tx = self.tx(owner).await;
        let created =
            store::create_draft(&mut tx, owner.author(), conversation, &content.version()).await;
        finish(tx, created).await.expect("create a draft")
    }

    /// The tool's next version of a draft.
    pub async fn write_draft(
        &self,
        owner: &Person,
        conversation: Uuid,
        draft: DashboardId,
        base_version: u32,
        rebase: Option<u32>,
        content: &Content,
    ) -> Result<VersionCreated, StoreError> {
        let mut tx = self.tx(owner).await;
        let written = store::write_draft(
            &mut tx,
            owner.author(),
            conversation,
            draft,
            base_version,
            rebase,
            &content.version(),
        )
        .await;
        finish(tx, written).await
    }

    /// The tool's draft of a saved dashboard.
    pub async fn fork(
        &self,
        owner: &Person,
        conversation: Uuid,
        saved: DashboardId,
        base_saved_version: u32,
        content: &Content,
    ) -> Result<VersionCreated, StoreError> {
        let mut tx = self.tx(owner).await;
        let forked = store::fork(
            &mut tx,
            owner.author(),
            conversation,
            saved,
            base_saved_version,
            &content.version(),
        )
        .await;
        finish(tx, forked).await
    }

    pub async fn publish(
        &self,
        person: &Person,
        draft: DashboardId,
        version: u32,
    ) -> Result<VersionCreated, StoreError> {
        let mut tx = self.tx(person).await;
        let published =
            store::publish(&mut tx, &self.user(person), draft, version, unlimited).await;
        finish(tx, published).await
    }

    /// A saved dashboard published by `editor` from a draft in a new conversation of theirs:
    /// the conversation, the draft and the saved dashboard (at version 1).
    pub async fn saved(
        &self,
        editor: &Person,
        content: &Content,
    ) -> (Uuid, DashboardId, DashboardId) {
        let conversation = self.conversation(editor).await;
        let draft = self.create_draft(editor, conversation, content).await;
        let saved = self
            .publish(editor, draft.dashboard_id, draft.version)
            .await
            .expect("publish the draft");
        assert_eq!(saved.version, 1);
        (conversation, draft.dashboard_id, saved.dashboard_id)
    }

    /// The public API in this process with `caps`, calling the stand-in `peers`.
    pub async fn api(&self, peers: &Peers, caps: RunCaps) -> Api {
        self.api_with(peers, caps, true).await
    }

    /// [`World::api`], without the token exchange when `exchange` is false (a deployment
    /// without `identity.exchange_url`).
    pub async fn api_with(&self, peers: &Peers, caps: RunCaps, exchange: bool) -> Api {
        let runner = Arc::new(DatasetRunner::new(Some(telemetry_client(peers)), caps));
        let routes = self.routes(peers, Arc::clone(&runner), exchange);
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("local address");
        let task = tokio::spawn(async move {
            axum::serve(listener, routes).await.expect("serve the API");
        });
        Api {
            url: format!("http://{address}"),
            runner,
            task,
        }
    }

    /// The public API's routes with `runner`, calling the stand-in `peers`, for a test that calls
    /// them in this process and decides when it reads an answer's body.
    pub fn routes(&self, peers: &Peers, runner: Arc<DatasetRunner>, exchange: bool) -> Router {
        self.routes_bounded(peers, runner, exchange, HELD_BODY_TIMEOUT)
    }

    /// [`World::routes`], whose run answers are cut off after `held_body_timeout` when the
    /// connection has not taken them whole by then.
    pub fn routes_bounded(
        &self,
        peers: &Peers,
        runner: Arc<DatasetRunner>,
        exchange: bool,
        held_body_timeout: Duration,
    ) -> Router {
        let identity = InternalClient::<service::Identity>::new(&peers.url, IDENTITY_TIMEOUT)
            .expect("a client for the exchange's stand-in");
        api::routes(
            self.issuer.verifier(ServiceName::Assistant),
            self.db.clone(),
            Arc::new(idle_agent(&peers.url)),
            Dashboards {
                on_behalf: exchange.then(|| OnBehalf::new(identity)),
                runner,
                versions: Arc::new(VersionBudget::default()),
                held_body_timeout,
            },
        )
    }
}

/// An agent without tools for the API's conversation routes, which no test here asks a
/// question of; its AI gateway is the stand-in at `peers_url`, which answers no model call.
fn idle_agent(peers_url: &str) -> Agent {
    let gateway = AnthropicGateway::new(peers_url, "main", 32 << 20).expect("a valid gateway URL");
    let registry = ToolRegistry::new(
        Vec::new(),
        ToolLimits {
            call_timeout: WAIT,
            max_calls_per_step: 8,
            max_text_bytes: 64 << 10,
            max_images: 4,
            max_image_bytes: 5 << 20,
        },
    )
    .expect("an empty tool set");
    Agent::new(
        Arc::new(gateway),
        registry,
        AgentSettings {
            effort: Effort::Medium,
            cache_ttl: CacheTtl::FiveMinutes,
            max_output_tokens: 16_000,
            call_timeout: WAIT,
            budget_tokens: 100_000_000,
            wind_down_percent: 80,
        },
    )
}

/// A client for telemetry-query's stand-in among `peers`.
pub fn telemetry_client(peers: &Peers) -> Callee {
    Callee::new(
        ServiceName::TelemetryQuery,
        &peers.url,
        Duration::from_secs(60),
    )
    .expect("a client for telemetry-query's stand-in")
}

/// The public API served in the test process.
pub struct Api {
    pub url: String,
    /// The runner the API uses, for runs as the `dashboard` tool makes them.
    pub runner: Arc<DatasetRunner>,
    task: JoinHandle<()>,
}

impl Drop for Api {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// An answer of the API.
#[derive(Debug)]
pub struct Answer {
    pub status: u16,
    pub headers: HeaderMap,
    pub text: String,
}

impl Answer {
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.text)
            .unwrap_or_else(|_| panic!("the answer is JSON: {}", self.text))
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|value| value.to_str().ok())
    }

    /// The M1 error's code and message key.
    pub fn error(&self) -> (String, String) {
        let body = self.json();
        (
            body["code"].as_str().unwrap_or_default().to_owned(),
            body["message_key"].as_str().unwrap_or_default().to_owned(),
        )
    }
}

/// A request to the API at `base`: `token` in `x-internal-identity` (none: without it), `body`
/// as JSON.
pub async fn call(
    base: &str,
    method: Method,
    path: &str,
    token: Option<&str>,
    body: Option<&Value>,
) -> Answer {
    let body = body.map(|body| serde_json::to_vec(body).expect("JSON"));
    call_raw(
        base,
        method,
        path,
        token,
        body.as_ref()
            .map(|body| ("application/json", body.as_slice())),
    )
    .await
}

/// A request to the API at `base` with `body` as it is, under its content type.
pub async fn call_raw(
    base: &str,
    method: Method,
    path: &str,
    token: Option<&str>,
    body: Option<(&str, &[u8])>,
) -> Answer {
    let mut request = reqwest::Client::new()
        .request(method, format!("{base}{path}"))
        .timeout(WAIT);
    if let Some(token) = token {
        request = request.header(INTERNAL_IDENTITY_HEADER, token);
    }
    if let Some((content_type, body)) = body {
        request = request
            .header(CONTENT_TYPE, content_type)
            .body(body.to_vec());
    }
    let response = request.send().await.expect("call the API");
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let text = response.text().await.expect("read the answer");
    Answer {
        status,
        headers,
        text,
    }
}

/// A call telemetry-query's stand-in received, with the claims of its token.
#[derive(Clone, Debug)]
pub struct TelemetryCall {
    pub path: String,
    pub body: Value,
    pub claims: InternalClaims,
}

impl TelemetryCall {
    pub fn tenant(&self) -> Option<TenantId> {
        self.claims.tenant.map(|grant| grant.id)
    }
}

/// How the stand-in answers a call: `None` for its usual answer, or a status and JSON body.
pub type Script = Arc<dyn Fn(TelemetryCall) -> BoxFuture<Option<(u16, Value)>> + Send + Sync>;

struct PeerState {
    issuer: Arc<TestIssuer>,
    /// The claims of every request token exchanged.
    exchanged: Mutex<Vec<InternalClaims>>,
    calls: Mutex<Vec<TelemetryCall>>,
    /// Every request to any other path: method and path.
    unexpected: Mutex<Vec<String>>,
    script: Mutex<Option<Script>>,
    /// The rows of a SQL answer.
    sql_rows: AtomicUsize,
}

/// The stand-in identity service and telemetry-query.
pub struct Peers {
    pub url: String,
    state: Arc<PeerState>,
    task: JoinHandle<()>,
}

impl Drop for Peers {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Peers {
    /// Starts the stand-ins; they trust tokens of `issuer` and issue with it.
    pub async fn start(issuer: Arc<TestIssuer>) -> Self {
        let state = Arc::new(PeerState {
            issuer,
            exchanged: Mutex::new(Vec::new()),
            calls: Mutex::new(Vec::new()),
            unexpected: Mutex::new(Vec::new()),
            script: Mutex::new(None),
            sql_rows: AtomicUsize::new(3),
        });
        let router = Router::new()
            .route(ExchangeToken::ROUTE, post(exchange))
            .route(SQL_PATH, post(telemetry))
            .route(STATISTICS_PATH, post(telemetry))
            .route(USAGE_PATH, post(telemetry))
            .fallback(unexpected)
            .with_state(Arc::clone(&state));
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("local address");
        let task = tokio::spawn(async move {
            axum::serve(listener, router)
                .await
                .expect("serve the peers");
        });
        Self {
            url: format!("http://{address}"),
            state,
            task,
        }
    }

    /// Answers telemetry calls with `script` from now on.
    pub fn script(&self, script: Script) {
        *lock(&self.state.script) = Some(script);
    }

    /// The rows a SQL answer has from now on.
    pub fn sql_rows(&self, rows: usize) {
        self.state.sql_rows.store(rows, Ordering::SeqCst);
    }

    pub fn calls(&self) -> Vec<TelemetryCall> {
        lock(&self.state.calls).clone()
    }

    pub fn exchanged(&self) -> Vec<InternalClaims> {
        lock(&self.state.exchanged).clone()
    }

    pub fn unexpected(&self) -> Vec<String> {
        lock(&self.state.unexpected).clone()
    }

    /// Waits until telemetry-query's stand-in received `count` calls.
    pub async fn wait_calls(&self, count: usize) {
        let deadline = Instant::now() + WAIT;
        while self.calls().len() < count {
            assert!(
                Instant::now() < deadline,
                "telemetry-query's stand-in did not receive {count} calls within {WAIT:?}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

fn refuse(code: ErrorCode) -> Response {
    let body = ErrorBody::new(code);
    (body.http_status(), axum::Json(body)).into_response()
}

fn token_of(headers: &HeaderMap) -> String {
    headers
        .get(INTERNAL_IDENTITY_HEADER)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned()
}

/// The token exchange, as the identity service answers a service calling on behalf of the
/// request's user.
async fn exchange(
    State(state): State<Arc<PeerState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Ok(user) = state
        .issuer
        .verifier(ServiceName::Assistant)
        .verify(&token_of(&headers))
    else {
        return refuse(ErrorCode::Unauthenticated);
    };
    let Ok(request) = serde_json::from_slice::<ExchangeToken>(&body) else {
        return refuse(ErrorCode::InvalidRequest);
    };
    let caller = user.claims().clone();
    lock(&state.exchanged).push(caller.clone());
    let iat = unix_now();
    let claims = InternalClaims {
        iss: ServiceName::Identity,
        sub: caller.sub,
        aud: request.audience,
        iat,
        exp: caller.exp.min(iat + 60),
        tenant: caller.tenant,
        platform_admin: caller.platform_admin,
        act: Some(Actor { sub: caller.aud }),
        sid: None,
    };
    axum::Json(json!({"token": state.issuer.token(&claims)})).into_response()
}

/// telemetry-query's SQL, statistics and usage routes.
async fn telemetry(
    State(state): State<Arc<PeerState>>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Ok(user) = state
        .issuer
        .verifier(ServiceName::TelemetryQuery)
        .verify(&token_of(&headers))
    else {
        return refuse(ErrorCode::Unauthenticated);
    };
    let Ok(body) = serde_json::from_slice::<Value>(&body) else {
        return refuse(ErrorCode::InvalidRequest);
    };
    let call = TelemetryCall {
        path: uri.path().to_owned(),
        body,
        claims: user.claims().clone(),
    };
    lock(&state.calls).push(call.clone());
    let script = lock(&state.script).clone();
    if let Some(script) = script
        && let Some((status, body)) = script(call.clone()).await
    {
        let status = StatusCode::from_u16(status).expect("a status");
        return (status, axum::Json(body)).into_response();
    }
    match call.path.as_str() {
        SQL_PATH => sql_answer(&call.body, state.sql_rows.load(Ordering::SeqCst)),
        STATISTICS_PATH => statistics_answer(&call.body),
        _ => usage_answer(&call.body),
    }
}

async fn unexpected(State(state): State<Arc<PeerState>>, method: Method, uri: Uri) -> Response {
    lock(&state.unexpected).push(format!("{method} {}", uri.path()));
    refuse(ErrorCode::Unavailable)
}

/// A SQL answer with `rows` days, as telemetry-query writes ClickHouse's values.
pub fn sql_answer(body: &Value, rows: usize) -> Response {
    let Ok(request) = serde_json::from_value::<RunSql>(body.clone()) else {
        return refuse(ErrorCode::InvalidRequest);
    };
    let now = Utc::now();
    let start = now.date_naive() - TimeDelta::days(i64::try_from(rows).expect("small"));
    let result = SqlResult {
        sql: request.sql,
        columns: vec![
            SqlColumn {
                name: "day".into(),
                data_type: "Date".into(),
            },
            SqlColumn {
                name: "kwh".into(),
                data_type: "Float64".into(),
            },
        ],
        rows: (0..rows)
            .map(|index| {
                let day = start + TimeDelta::days(i64::try_from(index).expect("small"));
                vec![json!(day.format("%Y-%m-%d").to_string()), json!(12.5)]
            })
            .collect(),
        queryable_from: QueryableFrom {
            tenant_telemetry: now - TimeDelta::days(7),
            tenant_telemetry_1m: now - TimeDelta::days(90),
            tenant_telemetry_1h: now - TimeDelta::days(90),
        },
        queried_at: now,
    };
    axum::Json(result).into_response()
}

/// A SQL answer to the request `body` of `rows` rows of two text cells, the second of
/// `cell_bytes` bytes: an answer and a result of about `rows` × `cell_bytes` bytes.
pub fn wide_sql_answer(body: &Value, rows: usize, cell_bytes: usize) -> Value {
    let now = Utc::now();
    let result = SqlResult {
        sql: body["sql"].as_str().unwrap_or_default().to_owned(),
        columns: vec![
            SqlColumn {
                name: "row".into(),
                data_type: "String".into(),
            },
            SqlColumn {
                name: "note".into(),
                data_type: "String".into(),
            },
        ],
        rows: (0..rows)
            .map(|index| vec![json!(index.to_string()), json!("x".repeat(cell_bytes))])
            .collect(),
        queryable_from: QueryableFrom {
            tenant_telemetry: now - TimeDelta::days(7),
            tenant_telemetry_1m: now - TimeDelta::days(90),
            tenant_telemetry_1h: now - TimeDelta::days(90),
        },
        queried_at: now,
    };
    serde_json::to_value(result).expect("a SQL answer as JSON")
}

/// The range of a semantic answer: the stand-in does not resolve periods.
fn answer_range(query: &SeriesQuery) -> QueryRange {
    let now = Utc::now().fixed_offset();
    QueryRange {
        tz: query.tz.clone(),
        interval: query.interval,
        from: now - TimeDelta::days(7),
        to: now,
        truncated: false,
        earliest: now - TimeDelta::days(90),
        ends_after_now: false,
        resolution: Resolution::Hour,
        queried_at: now,
    }
}

fn statistics_answer(body: &Value) -> Response {
    let Ok(query) = serde_json::from_value::<SeriesQuery>(body.clone()) else {
        return refuse(ErrorCode::InvalidRequest);
    };
    let now = Utc::now().fixed_offset();
    let series = query
        .series
        .iter()
        .map(|key| SeriesStatistics {
            device_id: key.device_id,
            point: key.point.clone(),
            status: DataStatus::Ok,
            count: 60,
            numeric_count: 60,
            min: Some(228.0),
            min_time: Some(now - TimeDelta::hours(2)),
            max: Some(231.0),
            max_time: Some(now - TimeDelta::hours(1)),
            avg: Some(229.5),
            first: None,
            last: None,
            highest: None,
            lowest: None,
            rows: vec![StatisticsRow {
                time: now - TimeDelta::days(7),
                count: 60,
                min: Some(228.0),
                max: Some(231.0),
                avg: Some(229.5),
                last: Reading::Float(Some(230.25)),
            }],
        })
        .collect();
    axum::Json(Statistics {
        range: answer_range(&query),
        series,
    })
    .into_response()
}

fn usage_answer(body: &Value) -> Response {
    let Ok(query) = serde_json::from_value::<SeriesQuery>(body.clone()) else {
        return refuse(ErrorCode::InvalidRequest);
    };
    let now = Utc::now().fixed_offset();
    let series = query
        .series
        .iter()
        .map(|key| SeriesUsage {
            device_id: key.device_id,
            point: key.point.clone(),
            status: UsageStatus::Ok,
            check: Some(CumulativeCheck {
                verdict: Verdict::Cumulative,
                drops: 0,
                rises: 47,
                comparisons: 47,
                from: now - TimeDelta::days(8),
                to: now,
            }),
            total: Some(91.5),
            drops: 0,
            readings: 864,
            first_reading: None,
            last_reading: None,
            baseline: true,
            gaps: 0,
            longest_gap: None,
            suspicious_steps: 0,
            largest_suspicious_step: None,
            skipped_cells: 0,
            highest: None,
            lowest: None,
            rows: (1..=3)
                .map(|day| UsageRow {
                    time: now - TimeDelta::days(day),
                    usage: 30.5,
                    readings: 288,
                    drops: 0,
                    after_gap: false,
                    suspicious_steps: 0,
                })
                .rev()
                .collect(),
        })
        .collect();
    axum::Json(Usage {
        range: answer_range(&query),
        series,
    })
    .into_response()
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
                lock(&lines).push(line);
            }
        });
    }

    /// Every line so far.
    pub fn text(&self) -> String {
        lock(&self.0).join("\n")
    }

    /// The JSON lines whose `message` is `message`.
    pub fn messages(&self, message: &str) -> Vec<Value> {
        lock(&self.0)
            .iter()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|value| value["message"] == message)
            .collect()
    }
}

/// Writes `jwks` to a file of its own under Cargo's temporary directory for these tests and
/// returns its path.
pub fn keys_file(jwks: &str) -> std::path::PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let name = format!(
        "assistant-jwks-{}-{}.json",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    );
    let path = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    std::fs::write(&path, jwks).expect("write the keys file");
    path
}

/// The assistant binary running as a child process.
pub struct Binary {
    child: Child,
    pub stdout: Output,
    pub stderr: Output,
    /// Where the public API listens, as a base URL.
    pub url: String,
    health: SocketAddr,
}

impl Binary {
    /// Starts the assistant on free local ports against `product` as `assistant_app` (and
    /// `assistant_platform` for the purge), trusting `issuer`'s keys, with every peer it calls
    /// (the identity service's exchange, telemetry-query, assets and the AI gateway) at
    /// `peers_url`, and waits until it is ready.
    /// The environment is otherwise empty, so the developer's own variables cannot leak in.
    pub async fn start(product: &ProductPostgres, issuer: &TestIssuer, peers_url: &str) -> Self {
        let keys_file = keys_file(&issuer.jwks());
        let mut command = Command::new(env!("CARGO_BIN_EXE_assistant"));
        command
            .env_clear()
            .env("TRUSTIOT__HTTP__LISTEN_ADDR", "127.0.0.1:0")
            .env("TRUSTIOT__HTTP__HEALTH_LISTEN_ADDR", "127.0.0.1:0")
            .env("TRUSTIOT__SHUTDOWN__DRAIN_DELAY", "100ms")
            .env("TRUSTIOT__SHUTDOWN__DRAIN_TIMEOUT", "5s")
            .env("TRUSTIOT_INTERNAL_IDENTITY_KEYS_FILE", &keys_file)
            .env("TRUSTIOT__DATABASE__USERNAME", "assistant_app")
            .env("TRUSTIOT__DATABASE__PASSWORD", TEST_PASSWORD)
            .env(
                "TRUSTIOT__DATABASE__PLATFORM_USERNAME",
                "assistant_platform",
            )
            .env("TRUSTIOT__DATABASE__PLATFORM_PASSWORD", TEST_PASSWORD)
            .env("TRUSTIOT__AI_GATEWAY__URL", peers_url)
            .env("TRUSTIOT__IDENTITY__EXCHANGE_URL", peers_url)
            .env("TRUSTIOT__TELEMETRY_QUERY__URL", peers_url)
            .env("TRUSTIOT__ASSETS__URL", peers_url)
            .envs(product.platform_env())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().expect("start the assistant");
        let (stdout, stderr) = (Output::default(), Output::default());
        stdout.collect(child.stdout.take().expect("piped stdout"));
        stderr.collect(child.stderr.take().expect("piped stderr"));
        let deadline = Instant::now() + WAIT;
        let listening = loop {
            if let Some(listening) = stdout.messages("listening").into_iter().next() {
                break listening;
            }
            if let Ok(Some(status)) = child.try_wait() {
                panic!(
                    "the assistant exited with {status}\nstdout:\n{}\nstderr:\n{}",
                    stdout.text(),
                    stderr.text()
                );
            }
            assert!(
                Instant::now() < deadline,
                "the assistant did not log `listening` within {WAIT:?}\nstdout:\n{}\nstderr:\n{}",
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
        let binary = Self {
            child,
            url: format!("http://{}", address("address")),
            health: address("health_address"),
            stdout,
            stderr,
        };
        binary.wait_ready().await;
        binary
    }

    /// Waits until `/readyz` answers 200.
    async fn wait_ready(&self) {
        let url = format!("http://{}/readyz", self.health);
        let deadline = Instant::now() + WAIT;
        loop {
            if let Ok(response) = reqwest::get(&url).await
                && response.status() == StatusCode::OK
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "/readyz of the assistant did not answer 200 within {WAIT:?}\nstdout:\n{}",
                self.stdout.text()
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

impl Drop for Binary {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}
