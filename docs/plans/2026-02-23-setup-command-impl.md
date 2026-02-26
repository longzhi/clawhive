# Setup Command Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Replace the one-shot `clawhive init` with a reentrant `clawhive setup` dashboard-based configuration manager.

**Architecture:** Refactor `init.rs` → `setup.rs` with a scan→dashboard→action loop. Reuse existing generate/prompt functions, add config scanning layer and YAML-aware mutation for main.yaml/routing.yaml. Replace `Commands::Init` with `Commands::Setup` in main.rs.

**Tech Stack:** Rust, dialoguer, console, serde_yaml

---

### Task 1: Rename init → setup and update Commands enum

**Files:**
- Rename: `crates/clawhive-cli/src/init.rs` → `crates/clawhive-cli/src/setup.rs`
- Modify: `crates/clawhive-cli/src/main.rs`
- Keep: `crates/clawhive-cli/src/init_ui.rs` (rename to `setup_ui.rs`)

**Step 1: Rename files**

Rename `init.rs` → `setup.rs` and `init_ui.rs` → `setup_ui.rs`.

**Step 2: Update main.rs module declarations and imports**

Change:
```rust
mod init;
mod init_ui;
use init::run_init;
```
To:
```rust
mod setup;
mod setup_ui;
use setup::run_setup;
```

**Step 3: Replace Commands::Init with Commands::Setup**

In the `Commands` enum, replace:
```rust
#[command(about = "Initialize clawhive configuration")]
Init {
    #[arg(long, help = "Force overwrite existing config")]
    force: bool,
},
```
With:
```rust
#[command(about = "Interactive configuration manager")]
Setup {
    #[arg(long, help = "Skip confirmation prompts on reconfigure/remove")]
    force: bool,
},
```

**Step 4: Update the match arm**

Replace:
```rust
Commands::Init { force } => {
    run_init(&cli.config_root, force).await?;
}
```
With:
```rust
Commands::Setup { force } => {
    run_setup(&cli.config_root, force).await?;
}
```

**Step 5: Update init_ui references in setup_ui.rs**

In `setup_ui.rs`, no changes needed to content — only the module path changed.

In `setup.rs`, update the import:
```rust
use crate::setup_ui::{print_done, print_logo, print_step, ARROW, HIVE};
```

**Step 6: Update test in main.rs**

Replace `parses_init_force_flag` test:
```rust
#[test]
fn parses_setup_force_flag() {
    let cli = Cli::try_parse_from(["clawhive", "setup", "--force"]).unwrap();
    assert!(matches!(cli.command, Commands::Setup { force: true }));
}
```

Also replace `init_ui_symbols_exist`:
```rust
#[test]
fn setup_ui_symbols_exist() {
    let _ = crate::setup_ui::CHECKMARK;
    let _ = crate::setup_ui::ARROW;
    let _ = crate::setup_ui::HIVE;
}
```

**Step 7: Update integration test**

In `tests/init_test.rs` (rename to `tests/setup_test.rs` if it exists), update any references from `init` to `setup`.

**Step 8: Run tests and verify**

Run: `cargo test -p clawhive-cli`
Expected: All existing tests pass with renamed symbols.

**Step 9: Commit**

```bash
git add -A
git commit -m "refactor: rename init → setup command"
```

---

### Task 2: Add config scanning (ConfigState)

**Files:**
- Create: `crates/clawhive-cli/src/setup_scan.rs`
- Modify: `crates/clawhive-cli/src/main.rs` (add `mod setup_scan;`)

**Step 1: Write tests for config scanning**

In `setup_scan.rs`, add tests at the bottom:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_empty_dir_returns_no_items() {
        let temp = tempfile::tempdir().unwrap();
        // Create minimal directory structure
        std::fs::create_dir_all(temp.path().join("config/providers.d")).unwrap();
        std::fs::create_dir_all(temp.path().join("config/agents.d")).unwrap();
        let state = scan_config(temp.path());
        assert!(state.providers.is_empty());
        assert!(state.agents.is_empty());
        assert!(state.channels.is_empty());
        assert!(state.default_agent.is_none());
    }

    #[test]
    fn scan_detects_provider_with_api_key() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("config/providers.d")).unwrap();
        std::fs::create_dir_all(temp.path().join("config/agents.d")).unwrap();
        std::fs::write(
            temp.path().join("config/providers.d/anthropic.yaml"),
            "provider_id: anthropic\nenabled: true\napi_base: https://api.anthropic.com/v1\napi_key_env: ANTHROPIC_API_KEY\nmodels:\n  - anthropic/claude-sonnet-4-5\n",
        ).unwrap();
        let state = scan_config(temp.path());
        assert_eq!(state.providers.len(), 1);
        assert_eq!(state.providers[0].provider_id, "anthropic");
        assert!(matches!(state.providers[0].auth_summary, AuthSummary::ApiKey { .. }));
    }

    #[test]
    fn scan_detects_provider_with_oauth() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("config/providers.d")).unwrap();
        std::fs::create_dir_all(temp.path().join("config/agents.d")).unwrap();
        std::fs::write(
            temp.path().join("config/providers.d/openai.yaml"),
            "provider_id: openai\nenabled: true\napi_base: https://api.openai.com/v1\napi_key_env: OPENAI_API_KEY\nauth_profile: \"openai-12345\"\nmodels:\n  - openai/gpt-4o-mini\n",
        ).unwrap();
        let state = scan_config(temp.path());
        assert_eq!(state.providers.len(), 1);
        assert!(matches!(state.providers[0].auth_summary, AuthSummary::OAuth { .. }));
    }

    #[test]
    fn scan_detects_agent() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("config/providers.d")).unwrap();
        std::fs::create_dir_all(temp.path().join("config/agents.d")).unwrap();
        std::fs::write(
            temp.path().join("config/agents.d/main.yaml"),
            "agent_id: main\nenabled: true\nidentity:\n  name: \"Bot\"\n  emoji: \"🤖\"\nmodel_policy:\n  primary: \"sonnet\"\n  fallbacks: []\n",
        ).unwrap();
        let state = scan_config(temp.path());
        assert_eq!(state.agents.len(), 1);
        assert_eq!(state.agents[0].agent_id, "main");
        assert_eq!(state.agents[0].name, "Bot");
        assert_eq!(state.agents[0].emoji, "🤖");
    }

    #[test]
    fn scan_detects_channels_from_main_yaml() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("config/providers.d")).unwrap();
        std::fs::create_dir_all(temp.path().join("config/agents.d")).unwrap();
        std::fs::write(
            temp.path().join("config/main.yaml"),
            "app:\n  name: test\n  env: dev\nruntime:\n  max_concurrent: 4\nfeatures:\n  multi_agent: true\n  sub_agent: true\n  tui: true\n  cli: true\nchannels:\n  telegram:\n    enabled: true\n    connectors:\n      - connector_id: tg-main\n        token: \"tok\"\n",
        ).unwrap();
        let state = scan_config(temp.path());
        assert_eq!(state.channels.len(), 1);
        assert_eq!(state.channels[0].connector_id, "tg-main");
        assert_eq!(state.channels[0].channel_type, "telegram");
    }

    #[test]
    fn scan_reads_default_agent_from_routing() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("config/providers.d")).unwrap();
        std::fs::create_dir_all(temp.path().join("config/agents.d")).unwrap();
        std::fs::write(
            temp.path().join("config/routing.yaml"),
            "default_agent_id: clawhive-main\nbindings: []\n",
        ).unwrap();
        let state = scan_config(temp.path());
        assert_eq!(state.default_agent.as_deref(), Some("clawhive-main"));
    }
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test -p clawhive-cli`
Expected: compile error (setup_scan module not found)

**Step 3: Implement ConfigState and scan_config**

Create `setup_scan.rs`:

```rust
use std::path::Path;
use serde::Deserialize;

#[derive(Debug, Clone)]
pub enum AuthSummary {
    ApiKey { env_var: String },
    OAuth { profile_name: String },
}

#[derive(Debug, Clone)]
pub struct ProviderInfo {
    pub provider_id: String,
    pub auth_summary: AuthSummary,
}

#[derive(Debug, Clone)]
pub struct AgentInfo {
    pub agent_id: String,
    pub name: String,
    pub emoji: String,
    pub primary_model: String,
}

#[derive(Debug, Clone)]
pub struct ChannelInfo {
    pub channel_type: String,
    pub connector_id: String,
}

#[derive(Debug, Clone)]
pub struct ConfigState {
    pub providers: Vec<ProviderInfo>,
    pub agents: Vec<AgentInfo>,
    pub channels: Vec<ChannelInfo>,
    pub default_agent: Option<String>,
}

// Internal deserialization structs for raw YAML (auth_profile not in ProviderConfig)
#[derive(Deserialize)]
struct RawProviderYaml {
    provider_id: String,
    #[serde(default)]
    api_key_env: String,
    #[serde(default)]
    auth_profile: Option<String>,
}

#[derive(Deserialize)]
struct RawAgentYaml {
    agent_id: String,
    #[serde(default)]
    identity: Option<RawIdentity>,
    model_policy: RawModelPolicy,
}

#[derive(Deserialize)]
struct RawIdentity {
    #[serde(default)]
    name: String,
    #[serde(default)]
    emoji: Option<String>,
}

#[derive(Deserialize)]
struct RawModelPolicy {
    primary: String,
}

pub fn scan_config(root: &Path) -> ConfigState {
    let providers = scan_providers(&root.join("config/providers.d"));
    let agents = scan_agents(&root.join("config/agents.d"));
    let (channels, default_agent) = scan_main_and_routing(
        &root.join("config/main.yaml"),
        &root.join("config/routing.yaml"),
    );

    ConfigState {
        providers,
        agents,
        channels,
        default_agent,
    }
}

fn scan_providers(dir: &Path) -> Vec<ProviderInfo> {
    let mut result = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return result,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "yaml" || e == "yml") {
            if let Ok(content) = std::fs::read_to_string(&path) {
                if let Ok(raw) = serde_yaml::from_str::<RawProviderYaml>(&content) {
                    let auth_summary = match raw.auth_profile {
                        Some(profile) => AuthSummary::OAuth { profile_name: profile },
                        None => AuthSummary::ApiKey { env_var: raw.api_key_env },
                    };
                    result.push(ProviderInfo {
                        provider_id: raw.provider_id,
                        auth_summary,
                    });
                }
            }
        }
    }
    result.sort_by(|a, b| a.provider_id.cmp(&b.provider_id));
    result
}

fn scan_agents(dir: &Path) -> Vec<AgentInfo> {
    let mut result = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return result,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "yaml" || e == "yml") {
            if let Ok(content) = std::fs::read_to_string(&path) {
                if let Ok(raw) = serde_yaml::from_str::<RawAgentYaml>(&content) {
                    let (name, emoji) = match raw.identity {
                        Some(id) => (id.name, id.emoji.unwrap_or_default()),
                        None => (raw.agent_id.clone(), String::new()),
                    };
                    result.push(AgentInfo {
                        agent_id: raw.agent_id,
                        name,
                        emoji,
                        primary_model: raw.model_policy.primary,
                    });
                }
            }
        }
    }
    result.sort_by(|a, b| a.agent_id.cmp(&b.agent_id));
    result
}

fn scan_main_and_routing(main_path: &Path, routing_path: &Path) -> (Vec<ChannelInfo>, Option<String>) {
    let mut channels = Vec::new();

    // Parse main.yaml for channels
    if let Ok(content) = std::fs::read_to_string(main_path) {
        if let Ok(main) = serde_yaml::from_str::<clawhive_core::MainConfig>(&content) {
            if let Some(tg) = &main.channels.telegram {
                if tg.enabled {
                    for c in &tg.connectors {
                        channels.push(ChannelInfo {
                            channel_type: "telegram".to_string(),
                            connector_id: c.connector_id.clone(),
                        });
                    }
                }
            }
            if let Some(dc) = &main.channels.discord {
                if dc.enabled {
                    for c in &dc.connectors {
                        channels.push(ChannelInfo {
                            channel_type: "discord".to_string(),
                            connector_id: c.connector_id.clone(),
                        });
                    }
                }
            }
        }
    }

    // Parse routing.yaml for default agent
    let default_agent = std::fs::read_to_string(routing_path)
        .ok()
        .and_then(|content| serde_yaml::from_str::<clawhive_core::RoutingConfig>(&content).ok())
        .map(|r| r.default_agent_id);

    (channels, default_agent)
}
```

**Step 4: Add `mod setup_scan;` to main.rs**

**Step 5: Run tests**

Run: `cargo test -p clawhive-cli`
Expected: All scan tests pass.

**Step 6: Commit**

```bash
git add -A
git commit -m "feat(setup): add config scanning layer"
```

---

### Task 3: Dashboard display and action menu loop

**Files:**
- Modify: `crates/clawhive-cli/src/setup.rs`
- Modify: `crates/clawhive-cli/src/setup_ui.rs`

**Step 1: Add dashboard rendering to setup_ui.rs**

Add these functions to `setup_ui.rs`:

```rust
use crate::setup_scan::{AuthSummary, ConfigState};

pub static CIRCLE: Emoji<'_, '_> = Emoji("○ ", "o ");

pub fn render_dashboard(term: &Term, state: &ConfigState) {
    let _ = term.clear_screen();
    print_logo(term);

    // Providers
    let _ = term.write_line(&format!("  {}", style("Providers").bold()));
    if state.providers.is_empty() {
        let _ = term.write_line(&format!("    {} {}", CIRCLE, style("none configured").dim()));
    } else {
        for p in &state.providers {
            let auth = match &p.auth_summary {
                AuthSummary::ApiKey { env_var } => format!("API key (env: {})", env_var),
                AuthSummary::OAuth { profile_name } => format!("OAuth ({})", profile_name),
            };
            let _ = term.write_line(&format!("    {} {:<14} {}", CHECKMARK, p.provider_id, style(auth).dim()));
        }
    }
    let _ = term.write_line("");

    // Agents
    let _ = term.write_line(&format!("  {}", style("Agents").bold()));
    if state.agents.is_empty() {
        let _ = term.write_line(&format!("    {} {}", CIRCLE, style("none configured").dim()));
    } else {
        for a in &state.agents {
            let _ = term.write_line(&format!(
                "    {} {:<16} {} {} ({})",
                CHECKMARK, a.agent_id, a.emoji, a.name, style(&a.primary_model).dim()
            ));
        }
    }
    let _ = term.write_line("");

    // Channels
    let _ = term.write_line(&format!("  {}", style("Channels").bold()));
    if state.channels.is_empty() {
        let _ = term.write_line(&format!("    {} {}", CIRCLE, style("none configured").dim()));
    } else {
        for c in &state.channels {
            let type_label = match c.channel_type.as_str() {
                "telegram" => "Telegram",
                "discord" => "Discord",
                other => other,
            };
            let _ = term.write_line(&format!("    {} {:<14} {}", CHECKMARK, c.connector_id, style(type_label).dim()));
        }
    }
    let _ = term.write_line("");

    // Routing
    let _ = term.write_line(&format!("  {}", style("Routing").bold()));
    match &state.default_agent {
        Some(agent) => {
            let _ = term.write_line(&format!("    {} default → {}", CHECKMARK, agent));
        }
        None => {
            let _ = term.write_line(&format!("    {} {}", CIRCLE, style("not configured").dim()));
        }
    }
    let _ = term.write_line("");
}
```

**Step 2: Define SetupAction enum and build_action_menu in setup.rs**

```rust
#[derive(Debug, Clone, PartialEq)]
enum SetupAction {
    AddProvider,
    AddAgent,
    AddChannel,
    ModifyExisting,
    RemoveExisting,
    Done,
}

fn build_action_labels(state: &ConfigState) -> Vec<(String, SetupAction)> {
    let mut actions = vec![
        ("Add provider".to_string(), SetupAction::AddProvider),
        ("Add agent".to_string(), SetupAction::AddAgent),
        ("Add channel".to_string(), SetupAction::AddChannel),
    ];

    let has_items = !state.providers.is_empty()
        || !state.agents.is_empty()
        || !state.channels.is_empty();

    if has_items {
        actions.push(("Modify existing…".to_string(), SetupAction::ModifyExisting));
        actions.push(("Remove existing…".to_string(), SetupAction::RemoveExisting));
    }

    actions.push(("Done".to_string(), SetupAction::Done));
    actions
}
```

**Step 3: Rewrite run_setup as the main loop**

Replace `run_init` with `run_setup`:

```rust
pub async fn run_setup(config_root: &Path, force: bool) -> Result<()> {
    let term = Term::stdout();
    let theme = ColorfulTheme::default();
    ensure_required_dirs(config_root)?;

    loop {
        let state = scan_config(config_root);
        render_dashboard(&term, &state);

        let actions = build_action_labels(&state);
        let labels: Vec<&str> = actions.iter().map(|(l, _)| l.as_str()).collect();

        let selected = Select::with_theme(&theme)
            .with_prompt("What would you like to do?")
            .items(&labels)
            .default(0)
            .interact()?;

        let action = &actions[selected].1;
        match action {
            SetupAction::AddProvider => handle_add_provider(config_root, &theme, &state, force).await?,
            SetupAction::AddAgent => handle_add_agent(config_root, &theme, &state, force)?,
            SetupAction::AddChannel => handle_add_channel(config_root, &theme, &state, force)?,
            SetupAction::ModifyExisting => handle_modify(config_root, &theme, &state, force).await?,
            SetupAction::RemoveExisting => handle_remove(config_root, &theme, &state, force)?,
            SetupAction::Done => break,
        }
    }

    term.write_line(&format!("{} Setup complete.", HIVE))?;
    Ok(())
}
```

**Step 4: Run tests**

Run: `cargo test -p clawhive-cli`
Expected: Compile succeeds, existing tests pass (handler functions will be stubs initially).

**Step 5: Commit**

```bash
git add -A
git commit -m "feat(setup): add dashboard display and action menu loop"
```

---

### Task 4: Implement Add Provider action

**Files:**
- Modify: `crates/clawhive-cli/src/setup.rs`

**Step 1: Implement handle_add_provider**

Reuse existing `prompt_provider`, `prompt_auth_choice`, `write_provider_config` but:
- If provider already exists in state, ask "X already configured. Reconfigure? [y/N]"
- Remove the `force` guard from `write_provider_config` — replace with the reconfigure prompt
- After write, print done message

```rust
async fn handle_add_provider(
    config_root: &Path,
    theme: &ColorfulTheme,
    state: &ConfigState,
    force: bool,
) -> Result<()> {
    let provider = prompt_provider(theme)?;
    let existing = state.providers.iter().any(|p| p.provider_id == provider.as_str());
    if existing && !force {
        let reconfigure = Confirm::with_theme(theme)
            .with_prompt(format!("{} already configured. Reconfigure?", provider.as_str()))
            .default(false)
            .interact()?;
        if !reconfigure {
            return Ok(());
        }
    }
    let auth = prompt_auth_choice(theme, provider).await?;
    write_provider_config_unchecked(config_root, provider, &auth)?;
    println!("{} Provider {} configured.", CHECKMARK, provider.as_str());
    Ok(())
}
```

Add `write_provider_config_unchecked` (same as `write_provider_config` but without the exists check):

```rust
fn write_provider_config_unchecked(config_root: &Path, provider: ProviderId, auth: &AuthChoice) -> Result<PathBuf> {
    let providers_dir = config_root.join("config/providers.d");
    fs::create_dir_all(&providers_dir)?;
    let target = providers_dir.join(format!("{}.yaml", provider.as_str()));
    let yaml = generate_provider_yaml(provider, auth);
    fs::write(&target, yaml)?;
    Ok(target)
}
```

**Step 2: Run tests**

Run: `cargo test -p clawhive-cli`
Expected: Pass.

**Step 3: Commit**

```bash
git add -A
git commit -m "feat(setup): implement Add Provider action"
```

---

### Task 5: Implement Add Agent action

**Files:**
- Modify: `crates/clawhive-cli/src/setup.rs`

**Step 1: Implement handle_add_agent**

Key change from old `prompt_agent_setup`: model list is dynamically generated from `state.providers` instead of a single provider.

```rust
fn handle_add_agent(
    config_root: &Path,
    theme: &ColorfulTheme,
    state: &ConfigState,
    force: bool,
) -> Result<()> {
    let agent_id: String = Input::with_theme(theme)
        .with_prompt("Agent ID")
        .default("clawhive-main".to_string())
        .interact_text()?;
    let agent_id = agent_id.trim().to_string();
    if agent_id.is_empty() {
        anyhow::bail!("agent id cannot be empty");
    }

    let existing = state.agents.iter().any(|a| a.agent_id == agent_id);
    if existing && !force {
        let reconfigure = Confirm::with_theme(theme)
            .with_prompt(format!("{agent_id} already configured. Reconfigure?"))
            .default(false)
            .interact()?;
        if !reconfigure {
            return Ok(());
        }
    }

    let name: String = Input::with_theme(theme)
        .with_prompt("Display name")
        .default("Clawhive".to_string())
        .interact_text()?;
    let emoji: String = Input::with_theme(theme)
        .with_prompt("Emoji")
        .default("🐝".to_string())
        .interact_text()?;

    // Dynamic model list from configured providers
    let mut models = Vec::new();
    for p in &state.providers {
        for m in provider_models_for_id(&p.provider_id) {
            models.push(m);
        }
    }
    if models.is_empty() {
        models.push("sonnet".to_string()); // fallback alias
    }
    models.push("Custom…".to_string());

    let model_labels: Vec<&str> = models.iter().map(String::as_str).collect();
    let selected = Select::with_theme(theme)
        .with_prompt("Primary model")
        .items(&model_labels)
        .default(0)
        .interact()?;

    let primary_model = if models[selected] == "Custom…" {
        Input::with_theme(theme)
            .with_prompt("Model ID (provider/model)")
            .interact_text()?
    } else {
        models[selected].clone()
    };

    write_agent_files_unchecked(config_root, &agent_id, &name, &emoji, &primary_model)?;
    println!("{} Agent {agent_id} configured.", CHECKMARK);
    Ok(())
}

fn provider_models_for_id(provider_id: &str) -> Vec<String> {
    match provider_id {
        "anthropic" => vec![
            "anthropic/claude-sonnet-4-5".to_string(),
            "anthropic/claude-3-haiku-20240307".to_string(),
        ],
        "openai" => vec!["openai/gpt-4o-mini".to_string(), "openai/gpt-4o".to_string()],
        _ => vec![],
    }
}

fn write_agent_files_unchecked(config_root: &Path, agent_id: &str, name: &str, emoji: &str, primary_model: &str) -> Result<()> {
    let agents_dir = config_root.join("config/agents.d");
    fs::create_dir_all(&agents_dir)?;
    let yaml = generate_agent_yaml(agent_id, name, emoji, primary_model);
    fs::write(agents_dir.join(format!("{agent_id}.yaml")), yaml)?;

    let prompt_dir = config_root.join("prompts").join(agent_id);
    fs::create_dir_all(&prompt_dir)?;
    let prompt_path = prompt_dir.join("system.md");
    if !prompt_path.exists() {
        fs::write(&prompt_path, default_system_prompt(name))?;
    }
    Ok(())
}
```

**Step 2: Run tests**

Run: `cargo test -p clawhive-cli`
Expected: Pass.

**Step 3: Commit**

```bash
git add -A
git commit -m "feat(setup): implement Add Agent action with dynamic model list"
```

---

### Task 6: Implement Add Channel action with routing update

**Files:**
- Modify: `crates/clawhive-cli/src/setup.rs`

**Step 1: Write test for add_channel_to_main_yaml**

```rust
#[test]
fn add_channel_to_existing_main_yaml_preserves_other_channels() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(temp.path().join("config")).unwrap();
    // Write initial main.yaml with telegram
    let initial = generate_main_yaml("clawhive", Some(ChannelConfig { connector_id: "tg-main".into(), token: "tok1".into() }), None);
    std::fs::write(temp.path().join("config/main.yaml"), &initial).unwrap();

    // Add discord channel
    add_channel_to_config(temp.path(), "discord", &ChannelConfig { connector_id: "dc-main".into(), token: "tok2".into() }).unwrap();

    let content = std::fs::read_to_string(temp.path().join("config/main.yaml")).unwrap();
    assert!(content.contains("tg-main")); // preserved
    assert!(content.contains("dc-main")); // added
}
```

**Step 2: Implement handle_add_channel**

```rust
fn handle_add_channel(
    config_root: &Path,
    theme: &ColorfulTheme,
    state: &ConfigState,
    _force: bool,
) -> Result<()> {
    let channel_types = ["Telegram", "Discord"];
    let selected = Select::with_theme(theme)
        .with_prompt("Channel type")
        .items(&channel_types)
        .default(0)
        .interact()?;
    let channel_type = match selected {
        0 => "telegram",
        _ => "discord",
    };
    let default_id = match channel_type {
        "telegram" => "tg-main",
        _ => "dc-main",
    };

    let connector_id: String = Input::with_theme(theme)
        .with_prompt("Connector ID")
        .default(default_id.to_string())
        .interact_text()?;

    let token = Password::with_theme(theme)
        .with_prompt("Bot token")
        .allow_empty_password(false)
        .interact()?;

    // Route to which agent?
    if state.agents.is_empty() {
        println!("  No agents configured yet. Add an agent first, then routing will be configured.");
    } else {
        let agent_labels: Vec<&str> = state.agents.iter().map(|a| a.agent_id.as_str()).collect();
        let agent_idx = Select::with_theme(theme)
            .with_prompt("Route messages to which agent?")
            .items(&agent_labels)
            .default(0)
            .interact()?;
        let target_agent = &state.agents[agent_idx].agent_id;

        add_routing_binding(config_root, channel_type, &connector_id, target_agent)?;
    }

    let cfg = ChannelConfig { connector_id: connector_id.clone(), token };
    add_channel_to_config(config_root, channel_type, &cfg)?;

    println!("{} Channel {connector_id} ({channel_type}) configured.", CHECKMARK);
    Ok(())
}
```

**Step 3: Implement add_channel_to_config (YAML mutation)**

This reads main.yaml, parses it, adds/updates the connector, writes back:

```rust
fn add_channel_to_config(config_root: &Path, channel_type: &str, cfg: &ChannelConfig) -> Result<()> {
    let main_path = config_root.join("config/main.yaml");

    if !main_path.exists() {
        // No main.yaml yet — generate fresh
        let tg = if channel_type == "telegram" { Some(cfg.clone()) } else { None };
        let dc = if channel_type == "discord" { Some(cfg.clone()) } else { None };
        let yaml = generate_main_yaml("clawhive", tg, dc);
        fs::write(&main_path, yaml)?;
        return Ok(());
    }

    // Parse existing, modify in-place using serde_yaml::Value
    let content = fs::read_to_string(&main_path)?;
    let mut doc: serde_yaml::Value = serde_yaml::from_str(&content)?;

    let channels = doc.get_mut("channels")
        .and_then(|c| c.as_mapping_mut())
        .ok_or_else(|| anyhow!("main.yaml missing channels section"))?;

    let connector_value = serde_yaml::to_value(&serde_yaml::Mapping::from_iter([
        (serde_yaml::Value::String("connector_id".into()), serde_yaml::Value::String(cfg.connector_id.clone())),
        (serde_yaml::Value::String("token".into()), serde_yaml::Value::String(cfg.token.clone())),
    ]))?;

    let channel_key = serde_yaml::Value::String(channel_type.to_string());
    match channels.get_mut(&channel_key) {
        Some(channel_section) => {
            // Channel section exists — update enabled and add/replace connector
            channel_section["enabled"] = serde_yaml::Value::Bool(true);
            let connectors = channel_section.get_mut("connectors")
                .and_then(|c| c.as_sequence_mut());
            match connectors {
                Some(seq) => {
                    // Remove existing connector with same ID, then add
                    seq.retain(|c| {
                        c.get("connector_id").and_then(|v| v.as_str()) != Some(&cfg.connector_id)
                    });
                    seq.push(connector_value);
                }
                None => {
                    channel_section["connectors"] = serde_yaml::Value::Sequence(vec![connector_value]);
                }
            }
        }
        None => {
            // No channel section — create it
            let mut section = serde_yaml::Mapping::new();
            section.insert(serde_yaml::Value::String("enabled".into()), serde_yaml::Value::Bool(true));
            section.insert(
                serde_yaml::Value::String("connectors".into()),
                serde_yaml::Value::Sequence(vec![connector_value]),
            );
            channels.insert(channel_key, serde_yaml::Value::Mapping(section));
        }
    }

    fs::write(&main_path, serde_yaml::to_string(&doc)?)?;
    Ok(())
}
```

**Step 4: Implement add_routing_binding**

```rust
fn add_routing_binding(config_root: &Path, channel_type: &str, connector_id: &str, agent_id: &str) -> Result<()> {
    let routing_path = config_root.join("config/routing.yaml");

    if !routing_path.exists() {
        let yaml = generate_routing_yaml(agent_id, None, None);
        fs::write(&routing_path, yaml)?;
        return Ok(());
    }

    let content = fs::read_to_string(&routing_path)?;
    let mut doc: serde_yaml::Value = serde_yaml::from_str(&content)?;

    let bindings = doc.get_mut("bindings")
        .and_then(|b| b.as_sequence_mut());

    let new_binding = serde_yaml::to_value(serde_yaml::Mapping::from_iter([
        ("channel_type".into(), serde_yaml::Value::String(channel_type.into())),
        ("connector_id".into(), serde_yaml::Value::String(connector_id.into())),
        ("match".into(), serde_yaml::to_value(serde_yaml::Mapping::from_iter([
            ("kind".into(), serde_yaml::Value::String("dm".into())),
        ])).unwrap()),
        ("agent_id".into(), serde_yaml::Value::String(agent_id.into())),
    ]))?;

    match bindings {
        Some(seq) => {
            // Remove existing binding for same connector, then add
            seq.retain(|b| {
                b.get("connector_id").and_then(|v| v.as_str()) != Some(connector_id)
            });
            seq.push(new_binding);
        }
        None => {
            doc["bindings"] = serde_yaml::Value::Sequence(vec![new_binding]);
        }
    }

    fs::write(&routing_path, serde_yaml::to_string(&doc)?)?;
    Ok(())
}
```

**Step 5: Run tests**

Run: `cargo test -p clawhive-cli`
Expected: All pass including the new YAML mutation test.

**Step 6: Commit**

```bash
git add -A
git commit -m "feat(setup): implement Add Channel with YAML mutation"
```

---

### Task 7: Implement Modify and Remove actions

**Files:**
- Modify: `crates/clawhive-cli/src/setup.rs`

**Step 1: Implement handle_modify**

```rust
async fn handle_modify(
    config_root: &Path,
    theme: &ColorfulTheme,
    state: &ConfigState,
    force: bool,
) -> Result<()> {
    let mut items: Vec<(String, &str)> = Vec::new(); // (label, type)
    for p in &state.providers {
        items.push((format!("{} (provider)", p.provider_id), "provider"));
    }
    for a in &state.agents {
        items.push((format!("{} (agent)", a.agent_id), "agent"));
    }
    for c in &state.channels {
        items.push((format!("{} (channel)", c.connector_id), "channel"));
    }
    items.push(("← Back".to_string(), "back"));

    let labels: Vec<&str> = items.iter().map(|(l, _)| l.as_str()).collect();
    let selected = Select::with_theme(theme)
        .with_prompt("Which item to modify?")
        .items(&labels)
        .default(0)
        .interact()?;

    match items[selected].1 {
        "provider" => {
            // Re-run provider setup for this provider
            handle_add_provider(config_root, theme, state, true).await?;
        }
        "agent" => {
            handle_add_agent(config_root, theme, state, true)?;
        }
        "channel" => {
            handle_add_channel(config_root, theme, state, force)?;
        }
        _ => {} // back
    }
    Ok(())
}
```

**Step 2: Implement handle_remove**

```rust
fn handle_remove(
    config_root: &Path,
    theme: &ColorfulTheme,
    state: &ConfigState,
    force: bool,
) -> Result<()> {
    let mut items: Vec<(String, &str, String)> = Vec::new(); // (label, type, id)
    for p in &state.providers {
        items.push((format!("{} (provider)", p.provider_id), "provider", p.provider_id.clone()));
    }
    for a in &state.agents {
        items.push((format!("{} (agent)", a.agent_id), "agent", a.agent_id.clone()));
    }
    for c in &state.channels {
        items.push((format!("{} (channel)", c.connector_id), "channel", c.connector_id.clone()));
    }
    items.push(("← Back".to_string(), "back", String::new()));

    let labels: Vec<&str> = items.iter().map(|(l, _, _)| l.as_str()).collect();
    let selected = Select::with_theme(theme)
        .with_prompt("Which item to remove?")
        .items(&labels)
        .default(0)
        .interact()?;

    let (_, item_type, item_id) = &items[selected];
    match *item_type {
        "provider" => {
            if state.providers.len() <= 1 {
                println!("  Cannot remove last provider.");
                return Ok(());
            }
            if !force {
                let confirm = Confirm::with_theme(theme)
                    .with_prompt(format!("Remove provider {}?", item_id))
                    .default(false)
                    .interact()?;
                if !confirm { return Ok(()); }
            }
            let path = config_root.join(format!("config/providers.d/{item_id}.yaml"));
            if path.exists() { fs::remove_file(&path)?; }
            println!("{} Provider {item_id} removed.", CHECKMARK);
        }
        "agent" => {
            if state.agents.len() <= 1 {
                println!("  Cannot remove last agent.");
                return Ok(());
            }
            if !force {
                let confirm = Confirm::with_theme(theme)
                    .with_prompt(format!("Remove agent {}?", item_id))
                    .default(false)
                    .interact()?;
                if !confirm { return Ok(()); }
            }
            let path = config_root.join(format!("config/agents.d/{item_id}.yaml"));
            if path.exists() { fs::remove_file(&path)?; }
            // Don't delete prompts dir — user may want to keep it
            println!("{} Agent {item_id} removed.", CHECKMARK);
        }
        "channel" => {
            if !force {
                let confirm = Confirm::with_theme(theme)
                    .with_prompt(format!("Remove channel {}?", item_id))
                    .default(false)
                    .interact()?;
                if !confirm { return Ok(()); }
            }
            remove_channel_from_config(config_root, item_id)?;
            remove_routing_binding(config_root, item_id)?;
            println!("{} Channel {item_id} removed.", CHECKMARK);
        }
        _ => {} // back
    }
    Ok(())
}
```

**Step 3: Implement remove_channel_from_config and remove_routing_binding**

```rust
fn remove_channel_from_config(config_root: &Path, connector_id: &str) -> Result<()> {
    let main_path = config_root.join("config/main.yaml");
    if !main_path.exists() { return Ok(()); }

    let content = fs::read_to_string(&main_path)?;
    let mut doc: serde_yaml::Value = serde_yaml::from_str(&content)?;

    if let Some(channels) = doc.get_mut("channels").and_then(|c| c.as_mapping_mut()) {
        for (_key, section) in channels.iter_mut() {
            if let Some(connectors) = section.get_mut("connectors").and_then(|c| c.as_sequence_mut()) {
                connectors.retain(|c| {
                    c.get("connector_id").and_then(|v| v.as_str()) != Some(connector_id)
                });
            }
        }
    }

    fs::write(&main_path, serde_yaml::to_string(&doc)?)?;
    Ok(())
}

fn remove_routing_binding(config_root: &Path, connector_id: &str) -> Result<()> {
    let routing_path = config_root.join("config/routing.yaml");
    if !routing_path.exists() { return Ok(()); }

    let content = fs::read_to_string(&routing_path)?;
    let mut doc: serde_yaml::Value = serde_yaml::from_str(&content)?;

    if let Some(bindings) = doc.get_mut("bindings").and_then(|b| b.as_sequence_mut()) {
        bindings.retain(|b| {
            b.get("connector_id").and_then(|v| v.as_str()) != Some(connector_id)
        });
    }

    fs::write(&routing_path, serde_yaml::to_string(&doc)?)?;
    Ok(())
}
```

**Step 4: Run tests**

Run: `cargo test -p clawhive-cli`
Expected: Pass.

**Step 5: Commit**

```bash
git add -A
git commit -m "feat(setup): implement Modify and Remove actions"
```

---

### Task 8: Clean up old init code and final integration

**Files:**
- Modify: `crates/clawhive-cli/src/setup.rs` (remove dead code from old init flow)
- Delete: `crates/clawhive-cli/tests/init_test.rs` if present, replace with `tests/setup_test.rs`

**Step 1: Remove dead code**

Remove `run_init`, `write_provider_config` (replaced by unchecked version), `write_agent_files` (replaced by unchecked), `write_main_and_routing` (replaced by mutation functions). Keep all `generate_*` functions and `prompt_*` functions.

**Step 2: Update integration test**

Rename `tests/init_test.rs` → `tests/setup_test.rs` if it exists. Update test to call `scan_config` and verify it reads generated config correctly.

**Step 3: Run full test suite**

Run: `cargo test --workspace`
Expected: All tests pass.

**Step 4: Run clippy**

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: Clean.

**Step 5: Commit**

```bash
git add -A
git commit -m "refactor(setup): remove dead init code, finalize setup command"
```
