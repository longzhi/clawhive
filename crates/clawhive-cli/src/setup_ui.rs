use console::{style, Emoji, Term};

use crate::setup_scan::{AuthSummary, ConfigState};

pub static CHECKMARK: Emoji<'_, '_> = Emoji("✅ ", "√ ");
pub static CIRCLE: Emoji<'_, '_> = Emoji("○ ", "o ");
pub static ARROW: Emoji<'_, '_> = Emoji("➜  ", "-> ");
pub static CRAB: Emoji<'_, '_> = Emoji("🦀 ", "");

pub fn print_logo(term: &Term) {
    let logo = r#"
  ┌─────────────────────────┐
  │    clawhive  setup      │
  └─────────────────────────┘
"#;
    let _ = term.write_line(&format!("{}", style(logo).cyan()));
}

pub fn print_done(term: &Term, msg: &str) {
    let _ = term.write_line(&format!("{} {}", CHECKMARK, style(msg).green()));
}

pub fn render_dashboard(term: &Term, state: &ConfigState) {
    let _ = term.write_line("");
    let _ = term.write_line(&format!(
        "{} {}",
        CRAB,
        style("Setup Dashboard").bold().cyan()
    ));

    let provider_marker = if state.providers.is_empty() {
        CIRCLE
    } else {
        CHECKMARK
    };
    let _ = term.write_line(&format!(
        "\n{} {}",
        provider_marker,
        style("Providers").bold()
    ));
    if state.providers.is_empty() {
        let _ = term.write_line("   not configured");
    } else {
        for provider in &state.providers {
            let auth = match &provider.auth_summary {
                AuthSummary::ApiKey => "api key".to_string(),
                AuthSummary::OAuth { profile_name } => format!("oauth ({profile_name})"),
            };
            let _ = term.write_line(&format!("   - {}: {}", provider.provider_id, auth));
        }
    }

    let agent_marker = if state.agents.is_empty() {
        CIRCLE
    } else {
        CHECKMARK
    };
    let _ = term.write_line(&format!("\n{} {}", agent_marker, style("Agents").bold()));
    if state.agents.is_empty() {
        let _ = term.write_line("   not configured");
    } else {
        for agent in &state.agents {
            let _ = term.write_line(&format!(
                "   - {} {} ({}) -> {}",
                agent.emoji, agent.name, agent.agent_id, agent.primary_model
            ));
        }
    }

    let channel_marker = if state.channels.is_empty() {
        CIRCLE
    } else {
        CHECKMARK
    };
    let _ = term.write_line(&format!(
        "\n{} {}",
        channel_marker,
        style("Channels").bold()
    ));
    if state.channels.is_empty() {
        let _ = term.write_line("   not configured");
    } else {
        for channel in &state.channels {
            let _ = term.write_line(&format!(
                "   - {} ({})",
                channel.connector_id, channel.channel_type
            ));
        }
    }

    let tools_marker = if state.tools.web_search_enabled {
        CHECKMARK
    } else {
        CIRCLE
    };
    let _ = term.write_line(&format!("\n{} {}", tools_marker, style("Tools").bold()));
    if state.tools.web_search_enabled {
        let provider = state
            .tools
            .web_search_provider
            .as_deref()
            .unwrap_or("unknown");
        let _ = term.write_line(&format!("   web_search: on ({provider})"));
    } else {
        let _ = term.write_line("   web_search: off");
    }

    let routing_marker = if state.default_agent.is_some() {
        CHECKMARK
    } else {
        CIRCLE
    };
    let _ = term.write_line(&format!("\n{} {}", routing_marker, style("Routing").bold()));
    match &state.default_agent {
        Some(agent_id) => {
            let _ = term.write_line(&format!("   default agent: {agent_id}"));
        }
        None => {
            let _ = term.write_line("   default agent not configured");
        }
    }
}
