# CLI Setup Wizard (clawhive init) Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Provide an interactive CLI wizard (`clawhive init`) that guides users through creating a working configuration, generating all necessary YAML files in the `config/` directory. The wizard should have a polished visual experience with colors, progress indicators, and styled output — similar to `create-next-app` or Astro's CLI.

**Architecture:** 
- Add a new `Init` subcommand to the CLI.
- Use `dialoguer` for interactive prompts (Select, Confirm, Input, Password).
- Use `console` crate for styled/colored terminal output (bold headers, colored step indicators, success/error styling).
- Implement a `Wizard` struct that handles the flow and data collection.
- Use template-based generation for `main.yaml`, `providers.d/`, `agents.d/`, `routing.yaml`, and `prompts/<agent_id>/system.md`.
- Provider auth supports both API Key and OAuth paths (PKCE for OpenAI, setup-token for Anthropic) — depends on `clawhive-auth` crate from the OAuth plan.
- Channel bot tokens are stored as plaintext in `main.yaml` (no env var indirection).
- Integration test will verify the generated files against the `load_config` logic in `clawhive-core`.

**Visual Style:**
- ASCII art logo at startup
- Colored step indicators: `[1/5]` with step title in bold
- Green checkmarks for completed steps, yellow arrows for current step
- Styled section headers and dividers between steps
- Summary panel at the end showing all generated files
- Colored success message with next-steps instructions

**Tech Stack:** 
- Rust
- clap 4.x (derive API)
- dialoguer 0.11 (interactive prompts)
- console 0.15 (terminal styling: colors, bold, emoji, Term)
- serde_yaml (serialization)
- anyhow (error handling)

---

### Task 1: Add Dependencies and Command Scaffold

**Files:**
- Modify: `crates/clawhive-cli/Cargo.toml`
- Modify: `crates/clawhive-cli/src/main.rs`

**Step 1: Add `dialoguer` and `console` to Cargo.toml**

Add `dialoguer = "0.11"`, `console = "0.15"`, and `serde_json` to dependencies. Note: `dialoguer` already depends on `console`, but we add it explicitly for direct use of styling APIs (`Style`, `Term`, `Emoji`).

**Step 2: Add `Init` subcommand to `Commands` enum**

```rust
#[derive(Subcommand)]
enum Commands {
    // ...
    #[command(about = "Initialize clawhive configuration")]
    Init {
        #[arg(long, help = "Force overwrite existing config")]
        force: bool,
    },
    // ...
}
```

**Step 3: Handle `Init` command in `main()`**

```rust
Commands::Init { force } => {
    run_init(&cli.config_root, force).await?;
}
```

**Step 4: Add UI helper module**

Create `crates/clawhive-cli/src/init_ui.rs` with reusable styling helpers:

```rust
use console::{style, Emoji, Term};

pub static CHECKMARK: Emoji<'_, '_> = Emoji("✅ ", "√ ");
pub static ARROW: Emoji<'_, '_> = Emoji("➜  ", "-> ");
pub static CRAB: Emoji<'_, '_> = Emoji("🦀 ", "");

pub fn print_logo(term: &Term) {
    term.write_line(&format!("{}", style("
  ┌─────────────────────────┐
  │    clawhive  setup       │
  └─────────────────────────┘
").cyan())).ok();
}

pub fn print_step(term: &Term, current: usize, total: usize, title: &str) {
    term.write_line(&format!(
        "\n{} {}",
        style(format!("[{}/{}]", current, total)).bold().cyan(),
        style(title).bold()
    )).ok();
}

pub fn print_done(term: &Term, msg: &str) {
    term.write_line(&format!("{} {}", CHECKMARK, style(msg).green())).ok();
}
```

**Step 5: Commit**

```bash
git add crates/clawhive-cli/Cargo.toml crates/clawhive-cli/src/main.rs crates/clawhive-cli/src/init_ui.rs
git commit -m "feat: add init subcommand scaffold with dialoguer and console"
```

### Task 2: Implement Provider Setup Step

**Files:**
- Create: `crates/clawhive-cli/src/init.rs`
- Modify: `crates/clawhive-cli/src/main.rs` (to export init module)

**Step 1: Define Wizard State and Provider Selection**

Print step header via `print_step(term, 1, 5, "LLM Provider")`. Use `dialoguer::Select` to pick between Anthropic and OpenAI.

**Step 2: Select Authentication Method**

After provider selection, prompt for auth method:

```
? Authentication method:
  > OAuth Login (use your subscription)
    API Key
```

- **OAuth path**: Trigger the corresponding flow from `clawhive-auth`:
  - OpenAI → PKCE OAuth flow (opens browser, local callback)
  - Anthropic → setup-token paste prompt
  - Store result in `auth-profiles.json` via `TokenManager`
  - Generate `providers.d/*.yaml` with `auth_profile: "<profile_name>"` reference
- **API Key path**: Prompt for API key (using `Password`), store in `providers.d/*.yaml` with `api_key_env` field

**Step 3: Implement Template Generation for Provider**

```rust
fn generate_provider_yaml(provider_id: &str, auth: &AuthChoice) -> String {
    match auth {
        AuthChoice::OAuth { profile_name } => format!(
            r#"provider_id: {provider_id}
enabled: true
api_base: {base}
auth_profile: "{profile_name}"
models:
  - {model}
"#,
            base = api_base(provider_id),
            model = default_model(provider_id),
        ),
        AuthChoice::ApiKey { env_var } => format!(
            r#"provider_id: {provider_id}
enabled: true
api_base: {base}
api_key_env: {env_var}
models:
  - {model}
"#,
            base = api_base(provider_id),
            model = default_model(provider_id),
        ),
    }
}
```

**Step 4: Write to `config/providers.d/<provider>.yaml`**

**Step 5: Commit**

```bash
git add crates/clawhive-cli/src/init.rs
git commit -m "feat: implement provider setup step with OAuth and API key paths"
```

> **Dependency note**: OAuth flow requires `clawhive-auth` crate (Tasks 1-5 of the OAuth plan). If implementing the wizard before OAuth is ready, the OAuth option can be gated behind a feature flag or shown as "coming soon".

### Task 3: Implement Agent and Identity Setup Step

**Files:**
- Modify: `crates/clawhive-cli/src/init.rs`

**Step 1: Prompt for Agent ID and Identity**

Prompt for `agent_id` (default: `clawhive-main`) and identity name/emoji.

**Step 2: Configure Model Policy**

Select primary model from the configured provider's models.

**Step 3: Implement Template Generation for Agent**

```yaml
agent_id: clawhive-main
enabled: true
identity:
  name: "Clawhive"
  emoji: "🦀"
model_policy:
  primary: "anthropic/claude-3-5-sonnet-latest"
  fallbacks: []
memory_policy:
  mode: "standard"
  write_scope: "all"
```

**Step 4: Write to `config/agents.d/<agent_id>.yaml`**

**Step 5: Generate Default Persona Prompts**

Create `prompts/<agent_id>/system.md` with a sensible default:

```markdown
You are {{agent_name}}, a helpful AI assistant powered by clawhive.

You are knowledgeable, concise, and friendly. When you don't know something, you say so honestly.
```

The wizard should create the file only if it doesn't already exist (respect `--force` flag for overwrite).

**Step 6: Commit**

```bash
git add crates/clawhive-cli/src/init.rs
git commit -m "feat: implement agent setup with default persona prompts"
```

### Task 4: Implement Routing and Channel Setup Step

**Files:**
- Modify: `crates/clawhive-cli/src/init.rs`

**Step 1: Prompt for Telegram/Discord enablement**

Use `Confirm` to ask if Telegram or Discord should be enabled.

**Step 2: Collect Channel Credentials**

Prompt for `connector_id` and bot token (using `Password` input for masking). Token is stored as **plaintext** directly in `main.yaml` — no env var indirection.

**Step 3: Generate `main.yaml` and `routing.yaml`**

Generate `main.yaml` with app name and channel configs. Bot tokens written directly:

```yaml
channels:
  telegram:
    connectors:
      - connector_id: main
        token: "7123456789:AAHxxxxxxxxxxxxxxxxxxxxxxxxxxxx"
```

Generate `routing.yaml` with `default_agent_id` and bindings for enabled channels.

**Step 4: Write files to `config/`**

**Step 5: Commit**

```bash
git add crates/clawhive-cli/src/init.rs
git commit -m "feat: implement routing and channel setup step"
```

### Task 5: Final Validation and Directory Creation

**Files:**
- Modify: `crates/clawhive-cli/src/init.rs`

**Step 1: Ensure directory structure exists**

Create `config/agents.d`, `config/providers.d`, `prompts/`, `skills/`, `data/`, `logs/`.

**Step 2: Call `clawhive validate` logic**

Import and run `load_config` from `clawhive-core` to verify the generated files.

**Step 3: Print Styled Success Summary**

Display a summary panel listing all generated files (with green checkmarks), followed by a boxed "Next Steps" section with colored instructions for setting env vars and running `clawhive start`. Example:

```
✅ Configuration complete!

  Generated files:
    ✅ config/main.yaml
    ✅ config/providers.d/anthropic.yaml
    ✅ config/agents.d/clawhive-main.yaml
    ✅ config/routing.yaml
    ✅ prompts/clawhive-main/system.md

  ┌─ Next Steps ───────────────────────────┐
  │                                         │
  │  1. Validate: clawhive validate         │
  │  2. Start:    clawhive start            │
  │                                         │
  │  (Optional) Edit your agent persona:    │
  │  prompts/clawhive-main/system.md        │
  │                                         │
  └─────────────────────────────────────────┘
```

Note: No environment variable setup needed — OAuth tokens are in `auth-profiles.json`, bot tokens are directly in `main.yaml`. If the user chose API Key auth, the only env var needed is the one referenced in `providers.d/*.yaml` (e.g. `ANTHROPIC_API_KEY`).

**Step 4: Commit**

```bash
git add crates/clawhive-cli/src/init.rs
git commit -m "feat: add final validation and directory creation to init"
```

### Task 6: Integration Testing for Wizard

**Files:**
- Create: `crates/clawhive-cli/tests/init_test.rs`

**Step 1: Write test that mocks user input (if possible) or verifies generated files**

Since `dialoguer` is interactive, unit tests should focus on the template generation logic. Integration tests can verify that a full "standard" config generated by the wizard is valid.

```rust
#[test]
fn test_standard_config_generation() {
    // Generate mock files in a temp dir
    // Call load_config(temp_dir)
    // Assert success
}
```

**Step 2: Run tests**

Run: `cargo test -p clawhive-cli`

**Step 3: Commit**

```bash
git add crates/clawhive-cli/tests/init_test.rs
git commit -m "test: add integration test for config generation"
```
