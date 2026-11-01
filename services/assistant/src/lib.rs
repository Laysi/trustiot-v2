//! The AI assistant (decision 31): the main agent loop that answers a user's question about
//! their tenant's data by calling tools, over an append-only conversation history laid out for
//! prompt caching, bounded by a token budget per question instead of a step limit. It calls
//! Claude only through the AI gateway (services/ai-gateway), behind one model interface
//! ([`model::ModelClient`], decision 15). Design: docs/architecture/ai-assistant.md; the
//! crate's layout and settings: README.md.
//!
//! The client API ([`api`]) serves under `/api/assistant/v1` a user's conversation threads
//! (Laysi/trustiot-v2#645: starting, listing, reading, renaming, archiving, deleting and
//! restoring them) and the dashboards (Laysi/trustiot-v2#150: listing, opening, publishing,
//! restoring and archiving them, and running their datasets). The scheduled [`purge`] removes
//! the conversations deleted more than 30 days ago.
//!
//! Where later work plugs in:
//! - asking a question in a conversation and streaming the answer (Laysi/trustiot-v2#646,
//!   Laysi/trustiot-v2#147): a handler in [`api::conversations`] around [`Agent::ask`], with
//!   [`store::start_run`] and [`store::append`]; [`Answer::removed`] says what [`answer_guard`]
//!   took out of an answer, for the chat to show [`answer_guard::CONTENT_REMOVED`];
//! - more resources of the API: a module under [`api`], merged in [`api::routes`] and listed in
//!   its OpenAPI document;
//! - the user's identity for tool calls during a long run (the run delegation of
//!   Laysi/trustiot-v2#551): the chat API starts a question's delegation with
//!   [`delegation::QuestionRuns::start`] while it handles the request, and hands the
//!   [`delegation::QuestionRun`] to [`Agent::ask`] (Laysi/trustiot-v2#591);
//! - more tools (the semantic layer, read-only SQL, dashboards): [`tools::Tool`] in the
//!   [`tools::ToolRegistry`], calling services through [`callee::Callee`];
//! - the database: [`start`] opens the service role's pool (Laysi/trustiot-v2#592) and readiness
//!   checks it and [`schema::SCHEMA`]; [`store`] keeps conversations and their append-only
//!   history (Laysi/trustiot-v2#553) in transactions of the conversation's tenant, which the
//!   API opens on that pool for each request;
//! - the per-call quota pre-deduction of the AI gateway's ledger (Laysi/trustiot-v2#140): each
//!   call's [`model::ModelRequest::call_id`] (the ledger's `llm_call_id`), and the gateway's
//!   refusal reaching the agent as a [`model::ModelError`];
//! - dashboards (Laysi/trustiot-v2#150): [`dashboards::store`] keeps them, the client API
//!   ([`api::dashboards`]) lists, opens, publishes, restores and archives them and runs their
//!   datasets as the viewer through [`Assistant::runner`], which the `dashboard` tool
//!   ([`tools::dashboard`]) shares; the tool checks pages with [`dashboards::checker`].

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use contract::client::InternalClient;
use contract::{ServiceName, service};
use service_kit::identity::{IdentityVerifier, OnBehalf, PublishedKeys, ServiceIdentity};
use service_kit::migrations::MigrationCheck;
use service_kit::tenant::{BackgroundTenantDb, PlatformDb, TenantDb};
use service_kit::{BoxError, Context};
use sqlx::postgres::PgPoolOptions;
use utoipa::OpenApi as _;

use crate::dashboards::caps::RunCaps;
use crate::dashboards::checker::{CHECK_CALL_TIMEOUT, Checker};
use crate::dashboards::runner::DatasetRunner;

pub mod agent;
pub mod answer_guard;
pub mod api;
pub mod budget;
pub mod callee;
pub mod config;
pub mod dashboards;
pub mod delegation;
pub mod history;
pub mod model;
pub mod prompt;
pub mod purge;
pub mod schema;
pub mod store;
pub mod tools;

pub use agent::{Agent, AgentError, AgentSettings, Answer};
pub use config::Config;

/// How long getting a database connection may take, opening a new one included.
pub const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(5);

/// Connections of the platform role's pool: the purge's one transaction at a time
/// ([`purge::round`]) and the readiness ping. The purge's job lock is on the service role's
/// pool (`config::MIN_DATABASE_CONNECTIONS`).
pub const PLATFORM_CONNECTIONS: u32 = 2;

/// The client API's OpenAPI document (merged into `api/openapi.json` by `tools/openapi`).
#[must_use]
pub fn openapi() -> utoipa::openapi::OpenApi {
    api::ApiDoc::openapi()
}

/// The service's startup for `service_kit::run`: opens the service role's pool lazily (with the
/// session check of `crates/service-kit`), builds the agent and its tools from the
/// configuration, which fails startup when they cannot be built, opens the platform role's pool
/// lazily, schedules the purge ([`purge::spawn`]), follows the identity service's public keys
/// file, and serves the client API ([`api::routes`]: the conversation threads and the
/// dashboards). Readiness passes while both roles (`assistant_app`, `assistant_platform`) can
/// open a checked session, so a purge that cannot log in shows, the service has a usable set of
/// public keys, and the database has every migration file this release expects
/// ([`schema::SCHEMA`]). It calls neither the AI gateway nor the services the tools call, so the
/// service is ready without a model. Asking questions (and so the run delegation) is
/// Laysi/trustiot-v2#646.
///
/// # Errors
///
/// When the agent cannot be built or the database settings would not verify the server. Nothing
/// is served then: the service exits with code 1.
pub async fn start(context: Context<Config>) -> Result<Router, BoxError> {
    let config = context.config();
    // One pool for the service role: the client API's transactions and the `dashboard` tool's,
    // which act for the question's tenant without a request.
    let (db, tool_db) = TenantDb::connect_lazy_with_background(
        config.service_role().connect_options(),
        PgPoolOptions::new()
            .max_connections(config.database.max_connections)
            .acquire_timeout(ACQUIRE_TIMEOUT),
        ServiceIdentity::new(ServiceName::Assistant),
    )?;
    let assistant = build(config, Some(tool_db))?;
    let platform = PlatformDb::connect_lazy(
        config.platform_role().connect_options(),
        PgPoolOptions::new()
            .max_connections(PLATFORM_CONNECTIONS)
            .acquire_timeout(ACQUIRE_TIMEOUT),
    )?;
    let (ping_db, ping_platform) = (db.clone(), platform.clone());
    context.health().add_check("postgres", move || {
        let (db, platform) = (ping_db.clone(), ping_platform.clone());
        async move {
            db.ping().await?;
            platform.ping().await
        }
    });
    MigrationCheck::new(&schema::SCHEMA, db.pool().clone()).register(context.health());
    purge::spawn(&context, db.pool().clone(), platform, config.purge.interval);
    let keys = PublishedKeys::watch(
        &context,
        config.platform.internal_identity_keys_file.clone(),
    )
    .await;
    let verifier = IdentityVerifier::new(ServiceName::Assistant, keys);
    // A dataset run exchanges the request's token for telemetry-query's; without the exchange
    // every run answers 503 `dashboard.dataset_failed`.
    let on_behalf = match &config.identity.exchange_url {
        Some(url) => Some(OnBehalf::new(InternalClient::<service::Identity>::new(
            url,
            delegation::IDENTITY_TIMEOUT,
        )?)),
        None => None,
    };
    tracing::info!(
        tools = assistant.agent.tool_count(),
        delegation = assistant.runs.is_some(),
        dataset_runs = on_behalf.is_some() && assistant.runner.is_configured(),
        "assistant ready"
    );
    Ok(api::routes(
        verifier,
        db,
        Arc::new(assistant.agent),
        api::dashboards::Dashboards {
            on_behalf,
            runner: assistant.runner,
            versions: Arc::new(dashboards::limits::VersionBudget::default()),
            held_body_timeout: api::held_body::HELD_BODY_TIMEOUT,
        },
    ))
}

/// The agent with its tools, how a question starts its delegation, and the dataset runs.
pub struct Assistant {
    pub agent: Agent,
    /// `None` without `identity.exchange_url`: the agent has no tools, and a question is asked
    /// with [`tools::NoDelegation`].
    pub runs: Option<delegation::QuestionRuns>,
    /// The dashboards' dataset runs, one per process with one set of caps: the public API and
    /// the `dashboard` tool share it. Without `telemetry_query.url` every run fails.
    pub runner: Arc<DatasetRunner>,
}

/// The assistant the configuration describes. The tools of a service are left out when its
/// address is not set; without the identity service's token exchange there are none, since a
/// tool only calls as the user. The `dashboard` tool needs `telemetry_query.url` (its datasets)
/// and `db` (where it writes); without `dashboard_check.url` (the check of custom pages) it
/// writes versions of components only and refuses custom pages (Laysi/trustiot-v2#934). Each
/// case is logged.
///
/// # Errors
///
/// When a client cannot be built from its address.
pub fn build(config: &Config, db: Option<BackgroundTenantDb>) -> Result<Assistant, BoxError> {
    let Some(exchange_url) = &config.identity.exchange_url else {
        tracing::info!(
            "no tools: identity.exchange_url is not set, so the assistant cannot call other \
             services as the user"
        );
        return Ok(Assistant {
            agent: build_agent(config, Vec::new())?,
            runs: None,
            runner: Arc::new(DatasetRunner::new(None, RunCaps::default())),
        });
    };
    let identity =
        InternalClient::<service::Identity>::new(exchange_url, delegation::IDENTITY_TIMEOUT)?;
    let runs = delegation::QuestionRuns::new(identity, config.identity.run_lifetime);
    let timeout = config.tools.call_timeout;
    let mut tools: Vec<Arc<dyn tools::Tool>> = Vec::new();
    let mut dataset_telemetry = None;
    let checker = match &config.dashboard_check.url {
        Some(url) => Some(Checker::new(url, CHECK_CALL_TIMEOUT)?),
        None => None,
    };
    match &config.telemetry_query.url {
        Some(url) => {
            let telemetry = callee::Callee::new(ServiceName::TelemetryQuery, url, timeout)?;
            dataset_telemetry = Some(telemetry.clone());
            tools.push(tools::telemetry::TelemetryLatest::new(telemetry.clone()));
            tools.push(tools::semantic::TelemetryStatistics::new(telemetry.clone()));
            tools.push(tools::semantic::TelemetryUsage::new(telemetry.clone()));
            // The read-only SQL of a cloud model (Laysi/trustiot-v2#143). On-prem models get the
            // semantic tools only (decision 16): leave this out for them when they come.
            tools.push(tools::telemetry_sql::TelemetrySql::new(telemetry));
        }
        None => tracing::info!("no telemetry tools: telemetry_query.url is not set"),
    }
    match &config.assets.url {
        Some(url) => {
            let assets = callee::Callee::new(ServiceName::Assets, url, timeout)?;
            tools.push(tools::device_models::DeviceModels::new(assets));
        }
        None => tracing::info!("no device model tool: assets.url is not set"),
    }
    let runner = Arc::new(DatasetRunner::new(dataset_telemetry, RunCaps::default()));
    match (db, runner.is_configured()) {
        (Some(db), true) => {
            if checker.is_none() {
                tracing::info!(
                    "the dashboard tool writes components only: dashboard_check.url is not set, \
                     so custom pages are refused"
                );
            }
            tools.push(tools::dashboard::DashboardTool::new(
                db,
                Arc::clone(&runner),
                checker,
            ));
        }
        (None, _) => tracing::info!("no dashboard tool: no database"),
        (_, false) => tracing::info!("no dashboard tool: telemetry_query.url is not set"),
    }
    Ok(Assistant {
        agent: build_agent(config, tools)?,
        runs: Some(runs),
        runner,
    })
}

/// The agent the configuration describes, with `tools`.
pub fn build_agent(config: &Config, tools: Vec<Arc<dyn tools::Tool>>) -> Result<Agent, BoxError> {
    let gateway = model::anthropic::AnthropicGateway::new(
        &config.ai_gateway.url,
        &config.ai_gateway.role,
        config.ai_gateway.max_response_bytes,
    )?;
    let registry = tools::ToolRegistry::new(
        tools,
        tools::ToolLimits {
            call_timeout: config.tools.call_timeout,
            max_calls_per_step: config.tools.max_calls_per_step,
            max_text_bytes: config.tools.max_text_bytes,
            max_images: config.tools.max_images,
            max_image_bytes: config.tools.max_image_bytes,
        },
    )?;
    Ok(Agent::new(
        Arc::new(gateway),
        registry,
        AgentSettings {
            effort: config.model.effort,
            cache_ttl: config.model.cache_ttl,
            max_output_tokens: config.model.max_output_tokens,
            call_timeout: config.model.call_timeout,
            budget_tokens: config.budget.tokens,
            wind_down_percent: config.budget.wind_down_percent,
        },
    ))
}
