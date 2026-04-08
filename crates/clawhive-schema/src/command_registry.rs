//! Centralized slash command registry — single source of truth for all platforms.
//!
//! Channel adapters (Telegram, Discord, etc.) read this registry to generate
//! platform-specific command registrations automatically.

/// Argument definition for a command.
#[derive(Debug, Clone)]
pub struct CommandArg {
    pub name: &'static str,
    pub description: &'static str,
    pub required: bool,
}

/// Platform-agnostic command definition.
///
/// - `name` uses space-separated format: `"skill list"`, `"skill remove"`.
/// - Commands sharing a prefix (e.g. `"skill *"`) are grouped as subcommands
///   on platforms that support it (Discord).
/// - Telegram converts spaces to underscores for its menu (`skill_list`).
#[derive(Debug, Clone)]
pub struct CommandDef {
    pub name: &'static str,
    pub description: &'static str,
    pub args: &'static [CommandArg],
}

impl CommandDef {
    /// Top-level command name (before first space). E.g. `"skill list"` → `"skill"`.
    pub fn root(&self) -> &str {
        self.name.split_once(' ').map_or(self.name, |(r, _)| r)
    }

    /// Subcommand name (after first space), if any. E.g. `"skill list"` → `Some("list")`.
    pub fn subcommand(&self) -> Option<&str> {
        self.name.split_once(' ').map(|(_, s)| s)
    }

    /// Telegram-style name with underscores. E.g. `"skill list"` → `"skill_list"`.
    pub fn telegram_name(&self) -> String {
        self.name.replace(' ', "_")
    }
}

/// All registered commands. This is the **single source of truth**.
///
/// Channel adapters call this to build platform-specific command menus.
pub fn command_registry() -> &'static [CommandDef] {
    static REGISTRY: &[CommandDef] = &[
        CommandDef {
            name: "new",
            description: "Start a fresh session",
            args: &[CommandArg {
                name: "model",
                description: "Model hint (e.g. opus, sonnet)",
                required: false,
            }],
        },
        CommandDef {
            name: "reset",
            description: "Start a fresh session",
            args: &[],
        },
        CommandDef {
            name: "stop",
            description: "Cancel the current task",
            args: &[],
        },
        CommandDef {
            name: "status",
            description: "Show session status",
            args: &[],
        },
        CommandDef {
            name: "model",
            description: "Show or change model",
            args: &[CommandArg {
                name: "model",
                description: "New model (e.g. openai/gpt-5.2)",
                required: false,
            }],
        },
        CommandDef {
            name: "help",
            description: "Show available commands",
            args: &[],
        },
        CommandDef {
            name: "skill list",
            description: "List installed skills",
            args: &[],
        },
        CommandDef {
            name: "skill analyze",
            description: "Analyze a skill before installing",
            args: &[CommandArg {
                name: "source",
                description: "Skill source (URL or path)",
                required: true,
            }],
        },
        CommandDef {
            name: "skill install",
            description: "Install a skill",
            args: &[CommandArg {
                name: "source",
                description: "Skill source (URL or path)",
                required: true,
            }],
        },
        CommandDef {
            name: "skill confirm",
            description: "Confirm a pending skill installation",
            args: &[CommandArg {
                name: "token",
                description: "Confirmation token",
                required: true,
            }],
        },
        CommandDef {
            name: "skill remove",
            description: "Remove an installed skill",
            args: &[CommandArg {
                name: "name",
                description: "Skill name",
                required: true,
            }],
        },
        CommandDef {
            name: "skill update",
            description: "Update a skill from its source",
            args: &[CommandArg {
                name: "name",
                description: "Skill name, or omit for all",
                required: false,
            }],
        },
    ];
    REGISTRY
}

/// Build a help text string from the command registry.
pub fn help_text() -> String {
    let mut lines = vec!["**Available Commands**".to_string()];
    for cmd in command_registry() {
        let args_str: String = cmd
            .args
            .iter()
            .map(|a| {
                if a.required {
                    format!(" <{}>", a.name)
                } else {
                    format!(" [{}]", a.name)
                }
            })
            .collect();
        lines.push(format!("/{}{} — {}", cmd.name, args_str, cmd.description));
    }
    lines.join("\n")
}

/// Generate Telegram underscore-to-space normalization pairs.
///
/// Returns pairs like `("/skill_list", "/skill list")` for commands containing spaces.
/// Telegram adapters apply these to incoming text before `parse_command`.
pub fn telegram_normalization_pairs() -> Vec<(String, String)> {
    command_registry()
        .iter()
        .filter(|cmd| cmd.name.contains(' '))
        .map(|cmd| {
            let underscore = format!("/{}", cmd.telegram_name());
            let spaced = format!("/{}", cmd.name);
            (underscore, spaced)
        })
        .collect()
}
