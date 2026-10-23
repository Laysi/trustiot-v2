//! The AI assistant (decision 31): the main agent loop that answers a user's question about
//! their tenant's data by calling tools, over an append-only conversation history laid out for
//! prompt caching, bounded by a token budget per question instead of a step limit. It calls
//! Claude only through the AI gateway (services/ai-gateway), behind one model interface
//! ([`model::ModelClient`], decision 15). Design: docs/architecture/ai-assistant.md; the
//! crate's layout and settings: README.md.
//!
//! Where later work plugs in:
//! - the chat API and its streaming (Laysi/trustiot-v2#146): [`Agent::start_conversation`] and
//!   [`Agent::ask`];
//! - the user's identity for tool calls during a long run (the run delegation of
//!   Laysi/trustiot-v2#551): the chat API starts a question's delegation with
//!   [`delegation::QuestionRuns::start`] while it handles the request, and hands the
//!   [`delegation::QuestionRun`] to [`Agent::ask`] (Laysi/trustiot-v2#591);
//! - more tools (the semantic layer, read-only SQL, dashboards): [`tools::Tool`] in the
//!   [`tools::ToolRegistry`], calling services through [`callee::Callee`];
//! - stored history (Laysi/trustiot-v2#553): [`history::Conversation`], append-only;
//! - the per-call quota pre-deduction of the AI gateway's ledger (Laysi/trustiot-v2#140): each
//!   call's [`model::ModelRequest::call_id`] (the ledger's `llm_call_id`), and the gateway's
//!   refusal reaching the agent as a [`model::ModelError`].

use std::sync::Arc;

use axum::Router;
use contract::client::InternalClient;
use contract::{ServiceName, service};
use service_kit::{BoxError, Context};

pub mod agent;
pub mod budget;
pub mod callee;
pub mod config;
pub mod delegation;
pub mod history;
pub mod model;
pub mod prompt;
pub mod tools;

pub use agent::{Agent, AgentError, AgentSettings, Answer};
pub use config::Config;

/// Builds the agent and its tools from the configuration, which fails startup when they cannot
/// be built. The service has no API of its own yet (the chat API is Laysi/trustiot-v2#146), so
/// it serves only service-kit's health endpoints, and the assistant is not kept.
pub async fn start(context: Context<Config>) -> Result<Router, BoxError> {
    let assistant = build(context.config())?;
    tracing::info!(
        tools = assistant.agent.tool_count(),
        delegation = assistant.runs.is_some(),
        "assistant ready"
    );
    Ok(Router::new())
}

/// The agent with its tools, and how a question starts its delegation.
pub struct Assistant {
    pub agent: Agent,
    /// `None` without `identity.exchange_url`: the agent has no tools, and a question is asked
    /// with [`tools::NoDelegation`].
    pub runs: Option<delegation::QuestionRuns>,
}

/// The assistant the configuration describes. The tools of a service are left out when its
/// address is not set; without the identity service's token exchange there are none, since a
/// tool only calls as the user. Each case is logged.
pub fn build(config: &Config) -> Result<Assistant, BoxError> {
    let Some(exchange_url) = &config.identity.exchange_url else {
        tracing::info!(
            "no tools: identity.exchange_url is not set, so the assistant cannot call other \
             services as the user"
        );
        return Ok(Assistant {
            agent: build_agent(config, Vec::new())?,
            runs: None,
        });
    };
    let identity =
        InternalClient::<service::Identity>::new(exchange_url, delegation::IDENTITY_TIMEOUT)?;
    let runs = delegation::QuestionRuns::new(identity, config.identity.run_lifetime);
    let timeout = config.tools.call_timeout;
    let mut tools: Vec<Arc<dyn tools::Tool>> = Vec::new();
    match &config.telemetry_query.url {
        Some(url) => {
            let telemetry = callee::Callee::new(ServiceName::TelemetryQuery, url, timeout)?;
            tools.push(tools::telemetry::TelemetrySeries::new(telemetry.clone()));
            tools.push(tools::telemetry::TelemetryLatest::new(telemetry));
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
    Ok(Assistant {
        agent: build_agent(config, tools)?,
        runs: Some(runs),
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
