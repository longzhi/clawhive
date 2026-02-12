use std::{collections::HashMap, fs, path::PathBuf};

use anyhow::Result;
use nanocrab_channels_telegram::TelegramAdapter;
use nanocrab_core::{AgentConfig, LlmRouter, Orchestrator};
use nanocrab_gateway::Gateway;
use nanocrab_provider::{register_builtin_providers, ProviderRegistry};

#[tokio::main]
async fn main() -> Result<()> {
    let root = PathBuf::from(".");
    let agent_path = root.join("config/agents.d/nanocrab-main.yaml");
    let text = fs::read_to_string(agent_path)?;
    let agent: AgentConfig = serde_yaml::from_str(&text)?;

    let mut registry = ProviderRegistry::new();
    register_builtin_providers(&mut registry);

    let aliases = HashMap::from([
        ("sonnet".to_string(), "anthropic/claude-sonnet-4-5".to_string()),
        ("opus".to_string(), "anthropic/claude-opus-4-6".to_string()),
        ("gpt".to_string(), "anthropic/claude-sonnet-4-5".to_string()),
    ]);

    let router = LlmRouter::new(registry, aliases, vec!["anthropic/claude-sonnet-4-5".to_string()]);
    let orchestrator = Orchestrator::new(router, vec![agent]);
    let gateway = Gateway::new(orchestrator, "nanocrab-main");

    let telegram = TelegramAdapter::new("tg_main");
    let inbound = telegram.to_inbound(10001, 42, "nanocrab bootstrap check");
    let outbound = gateway.handle_inbound(inbound).await?;

    println!("nanocrab CLI reply: {}", telegram.render_outbound(&outbound));
    Ok(())
}
