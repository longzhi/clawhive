use std::fs;

use anyhow::Result;
use nanocrab_core::load_config;
use uuid::Uuid;

fn unique_test_root() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("nanocrab-init-test-{}", Uuid::new_v4()))
}

#[test]
fn test_standard_config_generation() -> Result<()> {
    let root = unique_test_root();
    fs::create_dir_all(root.join("config/agents.d"))?;
    fs::create_dir_all(root.join("config/providers.d"))?;
    fs::create_dir_all(root.join("prompts/nanocrab-main"))?;

    fs::write(
        root.join("config/main.yaml"),
        r#"app:
  name: nanocrab
  env: dev

runtime:
  max_concurrent: 4

features:
  multi_agent: true
  sub_agent: true
  tui: true
  cli: true

channels:
  telegram:
    enabled: true
    connectors:
      - connector_id: main
        token: "7123456789:AAHxxxxxxxxxxxxxxxxxxxxxxxxxxxx"
  discord:
    enabled: false
    connectors: []

embedding:
  enabled: false
  provider: stub
  api_key_env: ""
  model: text-embedding-3-small
  dimensions: 1536
  base_url: https://api.openai.com/v1

tools: {}
"#,
    )?;

    fs::write(
        root.join("config/providers.d/anthropic.yaml"),
        r#"provider_id: anthropic
enabled: true
api_base: https://api.anthropic.com/v1
api_key_env: ANTHROPIC_API_KEY
models:
  - claude-sonnet-4-5
"#,
    )?;

    fs::write(
        root.join("config/agents.d/nanocrab-main.yaml"),
        r#"agent_id: nanocrab-main
enabled: true
identity:
  name: "Nanocrab"
  emoji: "🦀"
model_policy:
  primary: "anthropic/claude-sonnet-4-5"
  fallbacks: []
memory_policy:
  mode: "standard"
  write_scope: "all"
"#,
    )?;

    fs::write(
        root.join("config/routing.yaml"),
        r#"default_agent_id: nanocrab-main

bindings:
  - channel_type: telegram
    connector_id: main
    match:
      kind: dm
    agent_id: nanocrab-main
"#,
    )?;

    fs::write(
        root.join("prompts/nanocrab-main/system.md"),
        "You are Nanocrab, a helpful AI assistant powered by nanocrab.\n",
    )?;

    let config = load_config(&root.join("config"))?;
    assert_eq!(config.routing.default_agent_id, "nanocrab-main");
    assert_eq!(config.providers.len(), 1);
    assert_eq!(config.agents.len(), 1);
    assert_eq!(
        config.main.channels.telegram.as_ref().unwrap().connectors[0].token,
        "7123456789:AAHxxxxxxxxxxxxxxxxxxxxxxxxxxxx"
    );

    fs::remove_dir_all(root)?;
    Ok(())
}
