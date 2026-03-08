use std::path::Path;

use clawhive_core::config::SecurityMode;
use clawhive_core::load_config;
use console::style;

use crate::{is_process_running, read_pid_file};

pub fn print_status(root: &Path) {
    print_status_ex(root, false);
}

pub fn print_status_after_start(root: &Path) {
    print_status_ex(root, true);
}

/// Visible width of a string, ignoring ANSI escape codes.
fn visible_len(s: &str) -> usize {
    console::measure_text_width(s)
}

/// Render a table with box-drawing characters.
///   title: shown above the table
///   rows:  (label, value) pairs
fn render_table(title: &str, rows: &[(&str, String)]) {
    if rows.is_empty() {
        return;
    }

    let label_width = rows.iter().map(|(l, _)| l.len()).max().unwrap_or(0) + 1;
    let value_width = rows
        .iter()
        .map(|(_, v)| visible_len(v))
        .max()
        .unwrap_or(0)
        .max(10);

    let lw = label_width + 2; // padding
    let vw = value_width + 2;

    let top = format!("┌{}┬{}┐", "─".repeat(lw), "─".repeat(vw));
    let sep = format!("├{}┼{}┤", "─".repeat(lw), "─".repeat(vw));
    let bot = format!("└{}┴{}┘", "─".repeat(lw), "─".repeat(vw));

    println!("{title}");
    println!("{top}");

    for (i, (label, value)) in rows.iter().enumerate() {
        if i > 0 {
            println!("{sep}");
        }
        let val_visible = visible_len(value);
        let val_pad = (vw - 1).saturating_sub(val_visible);
        println!(
            "│ {:<width$}│ {}{} │",
            label,
            value,
            " ".repeat(val_pad),
            width = lw - 1
        );
    }

    println!("{bot}");
}

fn print_status_ex(root: &Path, just_started: bool) {
    let pid_info = match read_pid_file(root) {
        Ok(Some(pid)) if is_process_running(pid) => Some(pid),
        Ok(Some(_)) => None,
        _ => None,
    };

    let running = pid_info.is_some();

    // Status row
    let status_val = if running {
        let label = if just_started { "started" } else { "running" };
        format!("{} (pid: {})", style(label).green(), pid_info.unwrap())
    } else {
        style("stopped").red().to_string()
    };

    // Version
    let version = env!("CARGO_PKG_VERSION");

    // OS info
    let os_info = format!("{} ({})", os_version(), std::env::consts::ARCH);

    // Build rows
    let mut rows: Vec<(&str, String)> = vec![
        ("Status", status_val),
        ("Version", version.to_string()),
        ("OS", os_info),
    ];

    // Config-dependent rows
    let config_path = root.join("config");
    match load_config(&config_path) {
        Ok(config) => {
            // Agents
            let enabled_agents: Vec<_> = config.agents.iter().filter(|a| a.enabled).collect();
            let agent_parts: Vec<String> = enabled_agents
                .iter()
                .map(|a| {
                    let security_tag = match a.security {
                        SecurityMode::Off => format!(" {}", style("(no-security)").yellow()),
                        SecurityMode::Standard => String::new(),
                    };
                    format!(
                        "{} ({}){security_tag}",
                        style(&a.agent_id).cyan(),
                        a.model_policy.primary,
                    )
                })
                .collect();
            let agents_val = if agent_parts.is_empty() {
                style("none configured").yellow().to_string()
            } else {
                format!(
                    "{} configured · {}",
                    config.agents.len(),
                    agent_parts.join(", ")
                )
            };
            rows.push(("Agents", agents_val));

            // Providers
            let enabled_providers: Vec<_> = config.providers.iter().filter(|p| p.enabled).collect();
            let provider_parts: Vec<String> = enabled_providers
                .iter()
                .map(|p| {
                    let auth = if p.api_key.as_ref().is_some_and(|k| !k.is_empty()) {
                        style("✓ key set").green().to_string()
                    } else if p.auth_profile.is_some() {
                        style("✓ oauth").green().to_string()
                    } else {
                        style("✗ no key").yellow().to_string()
                    };
                    format!("{} ({})", style(&p.provider_id).cyan(), auth)
                })
                .collect();
            let providers_val = if provider_parts.is_empty() {
                style("none configured").yellow().to_string()
            } else {
                format!(
                    "{} configured · {}",
                    config.providers.len(),
                    provider_parts.join(", ")
                )
            };
            rows.push(("Providers", providers_val));

            // Channels
            let mut channels: Vec<String> = Vec::new();
            if let Some(tg) = &config.main.channels.telegram {
                let count = tg.connectors.len();
                channels.push(format!(
                    "{} ({} connector{})",
                    style("telegram").cyan(),
                    count,
                    if count != 1 { "s" } else { "" }
                ));
            }
            if let Some(dc) = &config.main.channels.discord {
                let count = dc.connectors.len();
                channels.push(format!(
                    "{} ({} connector{})",
                    style("discord").cyan(),
                    count,
                    if count != 1 { "s" } else { "" }
                ));
            }
            let channels_val = if channels.is_empty() {
                style("none configured").yellow().to_string()
            } else {
                channels.join(", ")
            };
            rows.push(("Channels", channels_val));

            // Routing
            let routing_val = format!(
                "{} binding{} · default: {}",
                config.routing.bindings.len(),
                if config.routing.bindings.len() != 1 {
                    "s"
                } else {
                    ""
                },
                style(&config.routing.default_agent_id).cyan()
            );
            rows.push(("Routing", routing_val));

            // Heartbeat
            let heartbeat_parts: Vec<String> = enabled_agents
                .iter()
                .map(|a| {
                    let hb = a.heartbeat.as_ref();
                    let status = match hb {
                        Some(h) if h.enabled => {
                            format!("{}m", h.interval_minutes)
                        }
                        _ => "off".to_string(),
                    };
                    format!("{} ({})", &a.agent_id, status)
                })
                .collect();
            if !heartbeat_parts.is_empty() {
                rows.push(("Heartbeat", heartbeat_parts.join(", ")));
            }

            // Security
            let security_val = format!(
                "max {} concurrent · embedding {}",
                config.main.runtime.max_concurrent,
                if config.main.embedding.enabled {
                    style("on").green().to_string()
                } else {
                    style("off").yellow().to_string()
                }
            );
            rows.push(("Runtime", security_val));
        }
        Err(_) => {
            rows.push((
                "Config",
                style("not found — run `clawhive setup`")
                    .yellow()
                    .to_string(),
            ));
        }
    }

    // Paths
    rows.push(("Config dir", root.join("config").display().to_string()));
    rows.push(("Data dir", root.join("data").display().to_string()));
    rows.push(("Log dir", root.join("logs").display().to_string()));

    println!();
    render_table("  Overview", &rows);
    println!();
}

fn os_version() -> String {
    #[cfg(target_os = "macos")]
    {
        use std::process::Command;
        Command::new("sw_vers")
            .arg("-productVersion")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|v| format!("macOS {}", v.trim()))
            .unwrap_or_else(|| "macOS".to_string())
    }
    #[cfg(target_os = "linux")]
    {
        use std::process::Command;
        Command::new("uname")
            .arg("-r")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|v| v.trim().to_string())
            .unwrap_or_else(|| "linux".to_string())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        std::env::consts::OS.to_string()
    }
}
