use std::path::Path;

use anyhow::Result;
use clawhive_core::config::{ClawhiveConfig, SecurityMode};
use clawhive_core::load_config;
use console::style;

/// Mask a secret string, showing first 4 and last 4 characters.
fn mask_token(s: &str) -> String {
    if s.len() <= 10 {
        return "****".to_string();
    }
    let prefix = &s[..4];
    let suffix = &s[s.len() - 4..];
    format!("{prefix}****{suffix}")
}

pub fn print_config(root: &Path) -> Result<()> {
    let config_path = root.join("config");
    let config = load_config(&config_path)?;
    print_config_detail(&config);
    Ok(())
}

fn print_config_detail(config: &ClawhiveConfig) {
    let main = &config.main;

    println!();
    println!("{}", style("═══ clawhive config ═══").bold().cyan());

    // ── App ──
    println!();
    println!("{}", style("app:").bold());
    println!("  name: {}", main.app.name);

    // ── Runtime ──
    println!();
    println!("{}", style("runtime:").bold());
    println!("  max_concurrent: {}", main.runtime.max_concurrent);

    // ── Features ──
    println!();
    println!("{}", style("features:").bold());
    println!("  multi_agent: {}", main.features.multi_agent);
    println!("  sub_agent: {}", main.features.sub_agent);
    println!("  tui: {}", main.features.tui);
    println!("  cli: {}", main.features.cli);

    // ── Channels ──
    println!();
    println!("{}", style("channels:").bold());
    if let Some(tg) = &main.channels.telegram {
        println!("  telegram:");
        println!("    enabled: {}", tg.enabled);
        println!("    connectors:");
        for c in &tg.connectors {
            println!("      - connector_id: {}", c.connector_id);
            println!("        token: {}", style(mask_token(&c.token)).yellow());
            println!("        require_mention: {}", c.require_mention);
        }
    }
    if let Some(dc) = &main.channels.discord {
        println!("  discord:");
        println!("    enabled: {}", dc.enabled);
        println!("    connectors:");
        for c in &dc.connectors {
            println!("      - connector_id: {}", c.connector_id);
            println!("        token: {}", style(mask_token(&c.token)).yellow());
            println!("        require_mention: {}", c.require_mention);
            if !c.groups.is_empty() {
                println!("        groups: {:?}", c.groups);
            }
        }
    }
    if main.channels.telegram.is_none() && main.channels.discord.is_none() {
        println!("  {}", style("(none configured)").yellow());
    }

    // ── Embedding ──
    println!();
    println!("{}", style("embedding:").bold());
    println!("  enabled: {}", main.embedding.enabled);
    println!("  provider: {}", main.embedding.provider);
    println!("  model: {}", main.embedding.model);
    if !main.embedding.api_key.is_empty() {
        println!(
            "  api_key: {}",
            style(mask_token(&main.embedding.api_key)).yellow()
        );
    }

    // ── Tools ──
    println!();
    println!("{}", style("tools:").bold());
    match &main.tools.web_search {
        Some(ws) => {
            println!("  web_search:");
            println!("    enabled: {}", ws.enabled);
            if let Some(p) = &ws.provider {
                println!("    provider: {p}");
            }
            if let Some(k) = &ws.api_key {
                println!("    api_key: {}", style(mask_token(k)).yellow());
            }
        }
        None => {
            println!("  web_search: {}", style("off").yellow());
        }
    }
    match &main.tools.actionbook {
        Some(ab) => println!("  actionbook: {}", if ab.enabled { "on" } else { "off" }),
        None => println!("  actionbook: {}", style("off").yellow()),
    }

    // ── Providers ──
    println!();
    println!("{}", style("providers:").bold());
    if config.providers.is_empty() {
        println!("  {}", style("(none configured)").yellow());
    }
    for p in &config.providers {
        println!("  - provider_id: {}", style(&p.provider_id).cyan());
        println!("    enabled: {}", p.enabled);
        println!("    api_base: {}", p.api_base);
        if let Some(key) = &p.api_key {
            if !key.is_empty() {
                println!("    api_key: {}", style(mask_token(key)).yellow());
            }
        }
        if let Some(profile) = &p.auth_profile {
            println!("    auth_profile: {profile}");
        }
        if !p.models.is_empty() {
            println!("    models: {:?}", p.models);
        }
    }

    // ── Agents ──
    println!();
    println!("{}", style("agents:").bold());
    if config.agents.is_empty() {
        println!("  {}", style("(none configured)").yellow());
    }
    for a in &config.agents {
        println!("  - agent_id: {}", style(&a.agent_id).cyan());
        println!("    enabled: {}", a.enabled);
        if let Some(identity) = &a.identity {
            println!(
                "    identity: {} {}",
                identity.emoji.as_deref().unwrap_or(""),
                identity.name
            );
        }
        println!("    model: {}", a.model_policy.primary);
        if !a.model_policy.fallbacks.is_empty() {
            println!("    fallbacks: {:?}", a.model_policy.fallbacks);
        }
        let security_label = match a.security {
            SecurityMode::Standard => "standard",
            SecurityMode::Off => "off",
        };
        println!("    security: {security_label}");
        if let Some(hb) = &a.heartbeat {
            println!(
                "    heartbeat: {} ({}m)",
                if hb.enabled { "on" } else { "off" },
                hb.interval_minutes
            );
        }
    }

    // ── Routing ──
    println!();
    println!("{}", style("routing:").bold());
    println!(
        "  default_agent_id: {}",
        style(&config.routing.default_agent_id).cyan()
    );
    if !config.routing.bindings.is_empty() {
        println!("  bindings:");
        for b in &config.routing.bindings {
            println!(
                "    - {} / {} ({}: {}) → {}",
                b.channel_type,
                b.connector_id,
                b.match_rule.kind,
                b.match_rule.pattern.as_deref().unwrap_or("*"),
                style(&b.agent_id).cyan()
            );
        }
    }

    println!();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_token_short() {
        assert_eq!(mask_token("abc"), "****");
        assert_eq!(mask_token("1234567890"), "****");
    }

    #[test]
    fn mask_token_long() {
        assert_eq!(mask_token("12345678901"), "1234****8901");
        assert_eq!(mask_token("sk-ant-api03-abcdefghijklmnop"), "sk-a****mnop");
    }
}
