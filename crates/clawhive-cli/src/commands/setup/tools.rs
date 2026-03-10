use std::path::Path;

use anyhow::Result;
use console::Term;
use dialoguer::{theme::ColorfulTheme, Confirm, Select};

use clawhive_core::config::{ActionbookConfig, WebSearchConfig};

use super::config_io::{input_or_back, load_main_config, save_main_config};
use super::ui::{print_done, ARROW};

pub(super) fn handle_configure_tools(config_root: &Path, theme: &ColorfulTheme) -> Result<()> {
    let term = Term::stdout();

    let enable_ws = Confirm::with_theme(theme)
        .with_prompt("Enable web search?")
        .default(true)
        .interact()?;

    let (provider, api_key) = if enable_ws {
        let ws_providers = ["Brave", "Tavily", "Serper", "← Back"];
        let selected = Select::with_theme(theme)
            .with_prompt("Web search provider")
            .items(&ws_providers)
            .default(0)
            .interact()?;
        if selected == ws_providers.len() - 1 {
            return Ok(());
        }
        let provider = ws_providers[selected].to_lowercase();
        let key = match input_or_back(theme, &format!("{} API key", ws_providers[selected]))? {
            Some(k) if !k.is_empty() => k,
            Some(_) => anyhow::bail!("API key cannot be empty"),
            None => return Ok(()),
        };
        (Some(provider), Some(key))
    } else {
        (None, None)
    };

    let mut cfg = load_main_config(config_root)?;
    cfg.tools.web_search = Some(WebSearchConfig {
        enabled: enable_ws,
        provider: provider.clone(),
        api_key: api_key.clone(),
    });
    save_main_config(config_root, &cfg)?;

    let ab_installed = clawhive_core::bin_exists("actionbook");
    let ab_prompt = if ab_installed {
        "Enable browser automation? (actionbook is installed)"
    } else {
        "Enable browser automation? (actionbook NOT installed)"
    };

    let enable_ab = Confirm::with_theme(theme)
        .with_prompt(ab_prompt)
        .default(ab_installed)
        .interact()?;

    if enable_ab && !ab_installed {
        term.write_line("")?;
        term.write_line(&format!(
            "  {} Actionbook CLI is required for browser automation.",
            ARROW
        ))?;
        term.write_line("  Install with one of:")?;
        term.write_line("")?;
        term.write_line("    curl -fsSL https://actionbook.dev/install.sh | bash")?;
        term.write_line("    npm install -g @actionbookdev/cli")?;
        term.write_line("")?;

        let install_now = Confirm::with_theme(theme)
            .with_prompt("Install actionbook now? (uses curl)")
            .default(true)
            .interact()?;

        if install_now {
            term.write_line("  Installing actionbook...")?;
            let status = std::process::Command::new("sh")
                .arg("-c")
                .arg("curl -fsSL https://actionbook.dev/install.sh | bash")
                .status();
            match status {
                Ok(s) if s.success() => {
                    print_done(&term, "actionbook installed successfully.");
                }
                _ => {
                    term.write_line(&format!(
                        "  {} Installation failed. Install manually later.",
                        ARROW
                    ))?;
                }
            }
        }
    }

    let mut cfg = load_main_config(config_root)?;
    cfg.tools.actionbook = Some(ActionbookConfig { enabled: enable_ab });
    save_main_config(config_root, &cfg)?;
    print_done(
        &Term::stdout(),
        &format!(
            "Tools configured. web_search: {}",
            if enable_ws { "on" } else { "off" }
        ),
    );
    Ok(())
}
