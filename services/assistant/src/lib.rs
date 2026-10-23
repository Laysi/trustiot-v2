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
//!   Laysi/trustiot-v2#551, wired in by Laysi/trustiot-v2#552): [`tools::Delegation`], handed
//!   to [`Agent::ask`];
//! - the real tools (Laysi/trustiot-v2#552): [`tools::Tool`] in the [`tools::ToolRegistry`];
//! - stored history (Laysi/trustiot-v2#553): [`history::Conversation`], append-only;
//! - the per-call quota pre-deduction of the AI gateway's ledger (Laysi/trustiot-v2#140): each
//!   call's [`model::ModelRequest::call_id`] (the ledger's `llm_call_id`), and the gateway's
//!   refusal reaching the agent as a [`model::ModelError`].

use std::sync::Arc;

use axum::Router;
use service_kit::{BoxError, Context};

pub mod agent;
pub mod budget;
pub mod config;
pub mod history;
pub mod model;
pub mod prompt;
pub mod tools;

pub use agent::{Agent, AgentError, AgentSettings, Answer};
pub use config::Config;

/// Builds the agent from the configuration, which fails startup when it cannot be built. The
/// service has no API of its own yet (the chat API is Laysi/trustiot-v2#146), so it serves only
/// service-kit's health endpoints, and the agent is not kept.
pub async fn start(context: Context<Config>) -> Result<Router, BoxError> {
    let agent = build_agent(context.config(), Vec::new())?;
    tracing::info!(tools = agent.tool_count(), "assistant ready");
    Ok(Router::new())
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
