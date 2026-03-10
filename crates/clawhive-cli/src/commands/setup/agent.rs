use std::fs;
use std::path::Path;

use anyhow::Result;
use console::Term;
use dialoguer::{theme::ColorfulTheme, Confirm, Select};

use super::config_io::{
    format_model_label, input_or_back, input_or_back_with_default, provider_models_for_id,
};
use super::scan::ConfigState;
use super::ui::print_done;

pub(super) fn handle_add_agent(
    config_root: &Path,
    theme: &ColorfulTheme,
    state: &ConfigState,
    force: bool,
) -> Result<()> {
    let agent_id = match input_or_back_with_default(theme, "Agent ID", "clawhive-main")? {
        Some(id) if !id.is_empty() => id,
        Some(_) => anyhow::bail!("agent id cannot be empty"),
        None => return Ok(()),
    };

    let existing = state.agents.iter().any(|a| a.agent_id == agent_id);
    if existing && !force {
        let actions = ["Reconfigure", "Remove", "Cancel"];
        let selected = Select::with_theme(theme)
            .with_prompt(format!("{agent_id} already configured"))
            .items(&actions)
            .default(2)
            .interact()?;
        match selected {
            0 => { /* continue to reconfigure below */ }
            1 => {
                if Confirm::with_theme(theme)
                    .with_prompt(format!("Are you sure you want to remove {agent_id}?"))
                    .default(false)
                    .interact()?
                {
                    let path = config_root.join(format!("config/agents.d/{agent_id}.yaml"));
                    if path.exists() {
                        fs::remove_file(&path)?;
                    }
                    print_done(&Term::stdout(), &format!("Agent {agent_id} removed."));
                }
                return Ok(());
            }
            _ => {
                return Ok(());
            }
        }
    }

    let name = match input_or_back_with_default(theme, "Display name", "Clawhive")? {
        Some(n) => n,
        None => return Ok(()),
    };
    let emoji = match input_or_back_with_default(theme, "Emoji", "🦀")? {
        Some(e) => e,
        None => return Ok(()),
    };

    let mut models = Vec::new();
    for p in &state.providers {
        for m in provider_models_for_id(&p.provider_id) {
            models.push(m);
        }
    }
    if models.is_empty() {
        models.push("sonnet".to_string());
    }
    models.push("Custom…".to_string());

    let model_labels: Vec<String> = models
        .iter()
        .map(|m| {
            if m == "Custom…" {
                m.clone()
            } else {
                format_model_label(m)
            }
        })
        .collect();
    let model_labels_refs: Vec<&str> = model_labels.iter().map(String::as_str).collect();
    let selected = Select::with_theme(theme)
        .with_prompt("Primary model")
        .items(&model_labels_refs)
        .default(0)
        .interact()?;

    let primary_model = if models[selected] == "Custom…" {
        match input_or_back(theme, "Model ID (provider/model)")? {
            Some(m) => m,
            None => return Ok(()),
        }
    } else {
        models[selected].clone()
    };

    // Thinking level selection
    let thinking_levels = ["None (default)", "Low", "Medium", "High"];
    let thinking_selected = Select::with_theme(theme)
        .with_prompt("Thinking level (for reasoning models)")
        .items(&thinking_levels)
        .default(0)
        .interact()?;
    let thinking_level = match thinking_selected {
        1 => Some("low"),
        2 => Some("medium"),
        3 => Some("high"),
        _ => None,
    };

    write_agent_files_unchecked(
        config_root,
        &agent_id,
        &name,
        &emoji,
        &primary_model,
        thinking_level,
    )?;
    print_done(&Term::stdout(), &format!("Agent {agent_id} configured."));
    Ok(())
}

fn write_agent_files_unchecked(
    config_root: &Path,
    agent_id: &str,
    name: &str,
    emoji: &str,
    primary_model: &str,
    thinking_level: Option<&str>,
) -> Result<()> {
    let agents_dir = config_root.join("config/agents.d");
    fs::create_dir_all(&agents_dir)?;
    let yaml = generate_agent_yaml(agent_id, name, emoji, primary_model, thinking_level);
    fs::write(agents_dir.join(format!("{agent_id}.yaml")), yaml)?;

    // Workspace prompt templates (AGENTS.md, SOUL.md, etc.) are created
    // automatically by workspace.init_with_defaults() during agent startup.
    Ok(())
}

fn generate_agent_yaml(
    agent_id: &str,
    name: &str,
    emoji: &str,
    primary_model: &str,
    thinking_level: Option<&str>,
) -> String {
    let thinking_line = match thinking_level {
        Some(level) => format!("\n  thinking_level: {level}"),
        None => String::new(),
    };
    format!(
        "agent_id: {agent_id}\nenabled: true\nidentity:\n  name: \"{name}\"\n  emoji: \"{emoji}\"\nmodel_policy:\n  primary: \"{primary_model}\"\n  fallbacks: []{thinking_line}\nmemory_policy:\n  mode: \"standard\"\n  write_scope: \"all\"\n"
    )
}

#[cfg(test)]
mod tests {
    use super::super::config_io::ensure_required_dirs;
    use super::*;

    #[test]
    fn agent_yaml_contains_identity_and_model_policy() {
        let yaml = generate_agent_yaml(
            "clawhive-main",
            "Clawhive",
            "\u{1F980}",
            "openai/gpt-4o-mini",
            None,
        );

        assert!(yaml.contains("agent_id: clawhive-main"));
        assert!(yaml.contains("name: \"Clawhive\""));
        assert!(yaml.contains("emoji: \"🦀\""));
        assert!(yaml.contains("primary: \"openai/gpt-4o-mini\""));
    }

    #[test]
    fn write_agent_files_unchecked_overwrites_yaml() {
        let temp = tempfile::tempdir().expect("create tempdir");
        ensure_required_dirs(temp.path()).expect("create required directories");

        let yaml_path = temp.path().join("config/agents.d/clawhive-main.yaml");
        std::fs::write(&yaml_path, "old: value\n").expect("write old agent yaml");

        write_agent_files_unchecked(
            temp.path(),
            "clawhive-main",
            "Clawhive",
            "\u{1F980}",
            "openai/gpt-4o-mini",
            None,
        )
        .expect("write agent files");

        let yaml = std::fs::read_to_string(&yaml_path).expect("read yaml");
        assert!(yaml.contains("agent_id: clawhive-main"));
        assert!(!yaml.contains("old: value"));
    }
}
