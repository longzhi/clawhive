use console::{style, Emoji, Term};

use super::scan::{AuthSummary, ConfigState};

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

/// Visible width of a string, ignoring ANSI escape codes.
fn visible_len(s: &str) -> usize {
    console::measure_text_width(s)
}

/// Render the setup dashboard as a box-drawing table.
pub fn render_dashboard(term: &Term, state: &ConfigState) {
    let mut rows: Vec<(&str, String)> = Vec::new();

    // Providers
    let providers_val = if state.providers.is_empty() {
        format!("{CIRCLE} not configured")
    } else {
        let parts: Vec<String> = state
            .providers
            .iter()
            .map(|p| {
                let auth = match &p.auth_summary {
                    AuthSummary::ApiKey => "api key".to_string(),
                    AuthSummary::OAuth { profile_name } => format!("oauth ({profile_name})"),
                };
                format!("{} ({auth})", style(&p.provider_id).cyan())
            })
            .collect();
        format!("{CHECKMARK}{}", parts.join(", "))
    };
    rows.push(("Providers", providers_val));

    // Agents
    let agents_val = if state.agents.is_empty() {
        format!("{CIRCLE} not configured")
    } else {
        let parts: Vec<String> = state
            .agents
            .iter()
            .map(|a| {
                format!(
                    "{} {} ({}) {ARROW}{}",
                    a.emoji,
                    style(&a.name).cyan(),
                    a.agent_id,
                    a.primary_model
                )
            })
            .collect();
        format!("{CHECKMARK}{}", parts.join(", "))
    };
    rows.push(("Agents", agents_val));

    // Channels
    let channels_val = if state.channels.is_empty() {
        format!("{CIRCLE} not configured")
    } else {
        let parts: Vec<String> = state
            .channels
            .iter()
            .map(|c| format!("{} ({})", style(&c.connector_id).cyan(), c.channel_type))
            .collect();
        format!("{CHECKMARK}{}", parts.join(", "))
    };
    rows.push(("Channels", channels_val));

    // Tools
    let mut tool_parts: Vec<String> = Vec::new();
    if state.tools.web_search_enabled {
        let provider = state
            .tools
            .web_search_provider
            .as_deref()
            .unwrap_or("unknown");
        tool_parts.push(format!("web_search: on ({provider})"));
    } else {
        tool_parts.push("web_search: off".to_string());
    }
    let ab_status = if state.tools.actionbook_enabled {
        if state.tools.actionbook_installed {
            "on (installed)"
        } else {
            "on (binary not found!)"
        }
    } else {
        "off"
    };
    tool_parts.push(format!("browser_automation: {ab_status}"));
    let tools_marker = if state.tools.web_search_enabled || state.tools.actionbook_enabled {
        CHECKMARK
    } else {
        CIRCLE
    };
    rows.push(("Tools", format!("{tools_marker}{}", tool_parts.join(", "))));

    // Routing
    let routing_val = match &state.default_agent {
        Some(agent_id) => format!("{CHECKMARK}default agent: {}", style(agent_id).cyan()),
        None => format!("{CIRCLE} default agent not configured"),
    };
    rows.push(("Routing", routing_val));

    // Render table
    let label_width = rows.iter().map(|(l, _)| l.len()).max().unwrap_or(0) + 1;
    let value_width = rows
        .iter()
        .map(|(_, v)| visible_len(v))
        .max()
        .unwrap_or(0)
        .max(10);

    let lw = label_width + 2;
    let vw = value_width + 2;

    let top = format!("┌{}┬{}┐", "─".repeat(lw), "─".repeat(vw));
    let sep = format!("├{}┼{}┤", "─".repeat(lw), "─".repeat(vw));
    let bot = format!("└{}┴{}┘", "─".repeat(lw), "─".repeat(vw));

    let title = format!("{CRAB}{}", style("Setup Dashboard").bold().cyan());
    let _ = term.write_line("");
    let _ = term.write_line(&format!("  {title}"));
    let _ = term.write_line(&top);

    for (i, (label, value)) in rows.iter().enumerate() {
        if i > 0 {
            let _ = term.write_line(&sep);
        }
        let val_visible = visible_len(value);
        let val_pad = vw.saturating_sub(val_visible + 2);
        let _ = term.write_line(&format!(
            "│ {:<width$}│ {}{} │",
            label,
            value,
            " ".repeat(val_pad),
            width = lw - 1
        ));
    }

    let _ = term.write_line(&bot);
}
