# Setup Command Redesign

Replace the one-shot `clawhive init` wizard with a reentrant `clawhive setup` configuration manager.

## Problem

`clawhive init` is all-or-nothing: it either crashes on existing files or `--force` overwrites everything. Users cannot incrementally add a provider, channel, or agent after initial setup without hand-editing YAML.

## Design

### Entry Point

```
clawhive setup [--force]
```

- `clawhive init` is **removed** (or aliased to `setup` with a deprecation notice).
- `--force` skips confirmation prompts on Reconfigure/Remove (power-user escape hatch).

### Flow

1. **Scan** — read `config/` directory, parse existing providers.d, agents.d, main.yaml, routing.yaml.
2. **Dashboard** — display current state in 4 sections.
3. **Action menu** — dynamically generated based on state.
4. **Execute** — run selected action's interactive flow.
5. **Write** — only touch the file(s) affected by that action.
6. **Loop** — refresh dashboard, return to menu. Repeat until user selects Done.

### Dashboard Display

```
🦀 clawhive configuration

  Providers
    ✓ anthropic    API key (env: ANTHROPIC_API_KEY)
    ✓ openai       OAuth (dragon@gmail.com)

  Agents
    ✓ clawhive-main   🤖 clawhive (claude-sonnet-4-5)

  Channels
    ✓ tg-main      Telegram
    ✓ dc-main      Discord

  Routing
    ✓ default → clawhive-main
```

Rules:
- Providers: one line per `providers.d/*.yaml`. Show auth type (OAuth + identifier, or API key + env var name).
- Agents: one line per `agents.d/*.yaml`. Show emoji + name + primary model.
- Channels: from `main.yaml` channels section. Show connector_id + type. Never display tokens.
- Routing: from `routing.yaml`. Show default agent binding only (read-only, not independently editable).
- Empty section: show `○ none configured`.

### Action Menu

```
What would you like to do?
  ❯ Add provider
    Add agent
    Add channel
    Modify existing…
    Remove existing…
    Done
```

- **Add** actions are always visible (providers/agents/channels all support multiples).
- **Modify existing…** opens sub-menu listing all configured items. Selecting one reruns that item's wizard flow and overwrites the corresponding file.
- **Remove existing…** opens sub-menu with confirmation. Deleting a channel also updates main.yaml and routing.yaml. Protection: last provider and last agent cannot be removed.
- **Done** exits.

### Add Provider

```
Which provider?
❯ Anthropic
  OpenAI

Authentication method?
❯ API Key (environment variable)
  OAuth Login

# API Key path:
Environment variable name: [ANTHROPIC_API_KEY]

# OAuth OpenAI path:
→ Opening browser for authorization…

# OAuth Anthropic path:
Paste your session token: ****

✓ Provider anthropic configured.
```

If chosen provider already exists: prompt "anthropic already configured. Reconfigure? [y/N]".
Writes: `providers.d/{provider_id}.yaml`.

### Add Agent

```
Agent ID: [my-agent]
Display name: [My Agent]
Emoji: [🤖]

Primary model?
❯ claude-sonnet-4-5 (Anthropic)
  gpt-4o-mini (OpenAI)
  Custom…

✓ Agent my-agent configured.
```

Model list is **dynamically generated** from configured providers. Unconfigured providers don't appear.
Writes: `agents.d/{agent_id}.yaml` + `prompts/{agent_id}/system.md` (only if not exists).

### Add Channel

```
Channel type?
❯ Telegram
  Discord

Connector ID: [tg-main]
Bot token: ****

Route messages to which agent?
❯ clawhive-main
  my-agent

✓ Channel tg-main configured, routed to clawhive-main.
```

Agent list is **dynamically generated** from configured agents.
Writes: `main.yaml` (channels section) + `routing.yaml` (bindings). Only the relevant connector/binding is added or updated; existing entries are preserved.

### Routing

Not independently editable. Routing is configured as part of the Add/Modify Channel flow ("Route to which agent?" prompt). Dashboard shows routing as read-only status.

## Implementation Notes

- Reuse existing prompt functions from `init.rs` (prompt_provider, prompt_agent_setup, prompt_channel_config) — refactor from write-all-at-end to write-per-action.
- Config scanning: use `load_config()` for validation, but also read individual YAML files directly for dashboard display (need provider auth details that load_config may normalize away).
- YAML mutation for main.yaml/routing.yaml: read → parse → modify section → write back. Do not regenerate from scratch.
- The `clawhive init` command should be replaced by `clawhive setup`. Remove the Init variant from Commands enum and add Setup.
