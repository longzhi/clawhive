use anyhow::{Context, Result};
use std::path::Path;

/// Controls which context files are included in the assembled system prompt.
///
/// | Mode      | AGENTS | SOUL | TOOLS | IDENTITY | USER | HEARTBEAT | BOOTSTRAP | MEMORY |
/// |-----------|--------|------|-------|----------|------|-----------|-----------|--------|
/// | Full      |   ✓    |  ✓   |   ✓   |    ✓     |  ✓   |     ✓     |     ✓     |   ✓    |
/// | Minimal   |   ✓    |  ✓   |   ✓   |    ✓     |  ✓   |           |           |        |
/// | Scheduled |   ✓    |  ✓   |   ✓   |    ✓     |  ✓   |           |           |   ✓    |
/// | Heartbeat |   ✓    |      |       |          |      |     ✓     |           |        |
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptMode {
    Full,
    Minimal,
    Scheduled,
    Heartbeat,
}

impl PromptMode {
    pub fn from_message_source(source: Option<&str>) -> Self {
        match source {
            Some("heartbeat") => Self::Heartbeat,
            Some("scheduled_task") => Self::Scheduled,
            _ => Self::Full,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Persona {
    pub agent_id: String,
    pub name: String,
    pub emoji: Option<String>,
    pub creature: Option<String>,
    pub vibe: Option<String>,
    pub role: Option<String>,
    pub agents_md: String,
    pub soul_md: String,
    pub user_md: String,
    pub identity_md: String,
    pub tools_md: String,
    pub heartbeat_md: String,
    pub bootstrap_md: String,
    pub memory_md: String,
}

impl Persona {
    pub fn assembled_system_prompt(&self) -> String {
        self.assembled_system_prompt_for_mode(PromptMode::Full)
    }

    pub fn assembled_system_prompt_minimal(&self) -> String {
        self.assembled_system_prompt_for_mode(PromptMode::Minimal)
    }

    pub fn assembled_system_prompt_heartbeat(&self) -> String {
        self.assembled_system_prompt_for_mode(PromptMode::Heartbeat)
    }

    pub fn assembled_system_prompt_scheduled(&self) -> String {
        self.assembled_system_prompt_for_mode(PromptMode::Scheduled)
    }

    pub fn assembled_system_prompt_for_mode(&self, mode: PromptMode) -> String {
        let mut parts = Vec::new();

        if !self.agents_md.is_empty() {
            parts.push(truncate_context_file(
                &self.agents_md,
                "AGENTS.md",
                MAX_CONTEXT_FILE_CHARS,
            ));
        }

        let mut context_files: Vec<(&str, String)> = Vec::new();

        // Heartbeat mode: only HEARTBEAT.md (lightweight, saves tokens)
        if mode == PromptMode::Heartbeat {
            if !self.heartbeat_md.is_empty() {
                context_files.push((
                    "HEARTBEAT.md",
                    truncate_context_file(
                        &self.heartbeat_md,
                        "HEARTBEAT.md",
                        MAX_CONTEXT_FILE_CHARS,
                    ),
                ));
            }
        } else {
            // All other modes include core context files: SOUL, TOOLS, IDENTITY, USER
            if !self.soul_md.is_empty() {
                context_files.push((
                    "SOUL.md",
                    truncate_context_file(&self.soul_md, "SOUL.md", MAX_CONTEXT_FILE_CHARS),
                ));
            }
            if !self.tools_md.is_empty() {
                context_files.push((
                    "TOOLS.md",
                    truncate_context_file(&self.tools_md, "TOOLS.md", MAX_CONTEXT_FILE_CHARS),
                ));
            }
            if !self.identity_md.is_empty() {
                context_files.push((
                    "IDENTITY.md",
                    truncate_context_file(&self.identity_md, "IDENTITY.md", MAX_CONTEXT_FILE_CHARS),
                ));
            }
            if !self.user_md.is_empty() {
                context_files.push((
                    "USER.md",
                    truncate_context_file(&self.user_md, "USER.md", MAX_CONTEXT_FILE_CHARS),
                ));
            }

            // Full mode: all files; Scheduled: skip HEARTBEAT + BOOTSTRAP; Minimal: skip HEARTBEAT + BOOTSTRAP + MEMORY
            if mode == PromptMode::Full {
                if !self.heartbeat_md.is_empty() {
                    context_files.push((
                        "HEARTBEAT.md",
                        truncate_context_file(
                            &self.heartbeat_md,
                            "HEARTBEAT.md",
                            MAX_CONTEXT_FILE_CHARS,
                        ),
                    ));
                }
                if !self.bootstrap_md.is_empty() {
                    context_files.push((
                        "BOOTSTRAP.md",
                        truncate_context_file(
                            &self.bootstrap_md,
                            "BOOTSTRAP.md",
                            MAX_CONTEXT_FILE_CHARS,
                        ),
                    ));
                }
            }
            if matches!(mode, PromptMode::Full | PromptMode::Scheduled)
                && !self.memory_md.is_empty()
            {
                context_files.push((
                    "MEMORY.md",
                    truncate_context_file(&self.memory_md, "MEMORY.md", MAX_CONTEXT_FILE_CHARS),
                ));
            }
        }

        if !context_files.is_empty() {
            let has_soul = context_files.iter().any(|(name, _)| *name == "SOUL.md");
            let has_tools = context_files.iter().any(|(name, _)| *name == "TOOLS.md");

            parts.push(
                "\n# Project Context\n\nThe following project context files have been loaded. \
                 Most live in `prompts/` (update via `write_file` with path `prompts/<filename>`). \
                 MEMORY.md lives at the workspace root."
                    .to_string(),
            );

            if has_soul {
                parts.push(
                    "If SOUL.md is present, embody its persona and tone. \
                     Avoid stiff, generic replies; follow its guidance unless higher-priority instructions override it."
                        .to_string(),
                );
            }
            if has_tools {
                parts.push(
                    "Note: TOOLS.md does not control tool availability; \
                     it is user-written guidance for how to use local environment resources."
                        .to_string(),
                );
            }

            let mut total_chars = 0;
            for (name, content) in &context_files {
                let remaining = TOTAL_MAX_CONTEXT_CHARS.saturating_sub(total_chars);
                if remaining == 0 {
                    parts.push(format!(
                        "\n## {name}\n\n[\u{2026}skipped, total context limit reached\u{2026}]"
                    ));
                    continue;
                }
                let clamped = if content.len() > remaining {
                    clamp_to_budget(content, remaining)
                } else {
                    content.clone()
                };
                total_chars += clamped.len();
                parts.push(format!("\n## {name}\n\n{clamped}"));
            }
        }

        // Heartbeat response rules (Full + Heartbeat modes, when HEARTBEAT.md has content)
        if matches!(mode, PromptMode::Full | PromptMode::Heartbeat)
            && !self.heartbeat_md.is_empty()
            && self.has_heartbeat_tasks()
        {
            parts.push(
                "\n## Heartbeats\n\
                 When you receive a heartbeat poll, read HEARTBEAT.md and follow it strictly.\n\
                 If nothing needs attention, reply exactly: HEARTBEAT_OK\n\
                 Do not include \"HEARTBEAT_OK\" in actual alert responses."
                    .to_string(),
            );
        }

        parts.join("\n")
    }

    /// Returns the heartbeat task content (may be empty).
    pub fn heartbeat_content(&self) -> &str {
        &self.heartbeat_md
    }

    /// Check if heartbeat has meaningful content (not just comments/whitespace).
    pub fn has_heartbeat_tasks(&self) -> bool {
        self.heartbeat_md.lines().any(|line| {
            let trimmed = line.trim();
            !trimmed.is_empty() && !trimmed.starts_with('#')
        })
    }
}

/// Load persona from workspace (OpenClaw-style).
/// Reads: {workspace}/prompts/AGENTS.md, SOUL.md, USER.md, IDENTITY.md, TOOLS.md, HEARTBEAT.md, BOOTSTRAP.md
/// and {workspace}/MEMORY.md (root, matching consolidation write path)
pub fn load_persona_from_workspace(
    workspace_root: &Path,
    agent_id: &str,
    name: &str,
    emoji: Option<&str>,
) -> Result<Persona> {
    let prompts_dir = workspace_root.join("prompts");

    let agents_md = read_optional_md(&prompts_dir.join("AGENTS.md"))
        .with_context(|| format!("loading AGENTS.md for {agent_id}"))?
        .unwrap_or_default();

    let soul_md = read_optional_md(&prompts_dir.join("SOUL.md"))?.unwrap_or_default();

    let user_md = read_optional_md(&prompts_dir.join("USER.md"))?.unwrap_or_default();

    let identity_md = read_optional_md(&prompts_dir.join("IDENTITY.md"))?.unwrap_or_default();

    let tools_md = read_optional_md(&prompts_dir.join("TOOLS.md"))?.unwrap_or_default();

    let heartbeat_md = read_optional_md(&prompts_dir.join("HEARTBEAT.md"))?.unwrap_or_default();

    let bootstrap_md = read_optional_md(&prompts_dir.join("BOOTSTRAP.md"))?.unwrap_or_default();

    // MEMORY.md lives at workspace root (written by consolidation), not in prompts/
    let memory_md = read_optional_md(&workspace_root.join("MEMORY.md"))?
        .or(read_optional_md(&workspace_root.join("memory.md"))?)
        .unwrap_or_default();

    // Parse IDENTITY.md for structured fields; values override YAML config fallbacks.
    // Changes take effect on config reload or restart, not on next session.
    let parsed_identity = crate::identity_parser::parse_identity_md(&identity_md);
    let resolved_name = parsed_identity.name.unwrap_or_else(|| name.to_string());
    let resolved_emoji = parsed_identity
        .emoji
        .or_else(|| emoji.map(|s| s.to_string()));

    Ok(Persona {
        agent_id: agent_id.to_string(),
        name: resolved_name,
        emoji: resolved_emoji,
        creature: parsed_identity.creature,
        vibe: parsed_identity.vibe,
        role: parsed_identity.role,
        agents_md,
        soul_md,
        user_md,
        identity_md,
        tools_md,
        heartbeat_md,
        bootstrap_md,
        memory_md,
    })
}

/// Legacy: Load persona from prompts directory (deprecated, for backward compatibility).
/// Reads: prompts/{agent_id}/system.md, style.md, safety.md
#[deprecated(note = "Use load_persona_from_workspace instead")]
pub fn load_persona(
    prompts_root: &Path,
    agent_id: &str,
    name: &str,
    emoji: Option<&str>,
) -> Result<Persona> {
    let dir = prompts_root.join(agent_id);

    let system_prompt = read_optional_md(&dir.join("system.md"))
        .with_context(|| format!("loading persona for {agent_id}"))?
        .unwrap_or_default();
    let style_prompt = read_optional_md(&dir.join("style.md"))?.unwrap_or_default();
    let safety_prompt = read_optional_md(&dir.join("safety.md"))?.unwrap_or_default();

    // Convert legacy format to new format
    let mut agents_md = system_prompt;
    if !style_prompt.is_empty() {
        agents_md.push_str(&format!("\n\n## Style\n{}", style_prompt));
    }
    if !safety_prompt.is_empty() {
        agents_md.push_str(&format!("\n\n## Safety\n{}", safety_prompt));
    }

    Ok(Persona {
        agent_id: agent_id.to_string(),
        name: name.to_string(),
        emoji: emoji.map(|s| s.to_string()),
        creature: None,
        vibe: None,
        role: None,
        agents_md,
        soul_md: String::new(),
        user_md: String::new(),
        identity_md: String::new(),
        tools_md: String::new(),
        heartbeat_md: String::new(),
        bootstrap_md: String::new(),
        memory_md: String::new(),
    })
}

/// Max characters per individual context file (OpenClaw DEFAULT_BOOTSTRAP_MAX_CHARS).
const MAX_CONTEXT_FILE_CHARS: usize = 20_000;
/// Max total characters across all context files (OpenClaw DEFAULT_BOOTSTRAP_TOTAL_MAX_CHARS).
const TOTAL_MAX_CONTEXT_CHARS: usize = 150_000;
const TRUNCATE_HEAD_RATIO: f64 = 0.7;
const TRUNCATE_TAIL_RATIO: f64 = 0.2;

/// Find the byte index at or before `target_chars` characters from the start,
/// snapping to a valid UTF-8 char boundary.
fn char_boundary_at(s: &str, target_chars: usize) -> usize {
    s.char_indices()
        .nth(target_chars)
        .map(|(idx, _)| idx)
        .unwrap_or(s.len())
}

/// Find the byte index at `target_chars` characters from the end,
/// snapping to a valid UTF-8 char boundary.
fn char_boundary_from_end(s: &str, target_chars: usize) -> usize {
    let total = s.chars().count();
    if target_chars >= total {
        return 0;
    }
    char_boundary_at(s, total - target_chars)
}

/// Truncate a context file to fit within `max_chars` (char count, not bytes),
/// preserving head (70%) and tail (20%). Uses Unicode-safe slicing.
fn truncate_context_file(content: &str, name: &str, max_chars: usize) -> String {
    let trimmed = content.trim_end();
    let char_count = trimmed.chars().count();
    if char_count <= max_chars {
        return trimmed.to_string();
    }
    let head_chars = (max_chars as f64 * TRUNCATE_HEAD_RATIO) as usize;
    let tail_chars = (max_chars as f64 * TRUNCATE_TAIL_RATIO) as usize;
    let head_end = char_boundary_at(trimmed, head_chars);
    let tail_start = char_boundary_from_end(trimmed, tail_chars);
    let head = &trimmed[..head_end];
    let tail = &trimmed[tail_start..];
    format!(
        "{head}\n\n[\u{2026}truncated, read {name} for full content\u{2026}]\n\
         \u{2026}(truncated {name}: kept {head_chars}+{tail_chars} chars of {char_count})\u{2026}\n\n{tail}"
    )
}

/// Clamp content to a character budget, appending \u{2026} if truncated.
/// Matches OpenClaw's `clampToBudget()` for second-pass truncation.
fn clamp_to_budget(content: &str, budget: usize) -> String {
    let char_count = content.chars().count();
    if char_count <= budget {
        return content.to_string();
    }
    if budget == 0 {
        return String::new();
    }
    let safe = budget.saturating_sub(1);
    let end = char_boundary_at(content, safe);
    format!("{}\u{2026}", &content[..end])
}

fn read_optional_md(path: &Path) -> Result<Option<String>> {
    if path.exists() {
        Ok(Some(std::fs::read_to_string(path)?))
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn load_persona_from_workspace_reads_files() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let prompts_dir = root.join("prompts");
        std::fs::create_dir_all(&prompts_dir).unwrap();

        std::fs::write(prompts_dir.join("AGENTS.md"), "Be helpful.").unwrap();
        std::fs::write(prompts_dir.join("SOUL.md"), "Be warm.").unwrap();
        std::fs::write(prompts_dir.join("USER.md"), "Name: Test User").unwrap();
        std::fs::write(prompts_dir.join("IDENTITY.md"), "Name: TestBot").unwrap();
        std::fs::write(prompts_dir.join("TOOLS.md"), "SSH: localhost").unwrap();
        std::fs::write(prompts_dir.join("HEARTBEAT.md"), "- Check email").unwrap();

        let persona = load_persona_from_workspace(root, "test", "TestBot", Some("🤖")).unwrap();

        assert_eq!(persona.agent_id, "test");
        assert_eq!(persona.name, "TestBot");
        assert_eq!(persona.emoji, Some("🤖".to_string()));
        assert!(persona.agents_md.contains("Be helpful"));
        assert!(persona.soul_md.contains("Be warm"));
        assert!(persona.user_md.contains("Test User"));
        assert!(persona.identity_md.contains("TestBot"));
        assert!(persona.tools_md.contains("SSH"));
        assert!(persona.heartbeat_md.contains("Check email"));
    }

    #[test]
    fn load_persona_missing_files_fallback_empty() {
        let tmp = TempDir::new().unwrap();
        let persona = load_persona_from_workspace(tmp.path(), "test", "Test", None).unwrap();

        assert!(persona.agents_md.is_empty());
        assert!(persona.soul_md.is_empty());
        assert!(persona.heartbeat_md.is_empty());
    }

    fn make_persona() -> Persona {
        Persona {
            agent_id: "test".into(),
            name: "Test".into(),
            emoji: None,
            creature: None,
            vibe: None,
            role: None,
            agents_md: String::new(),
            soul_md: String::new(),
            user_md: String::new(),
            identity_md: String::new(),
            tools_md: String::new(),
            heartbeat_md: String::new(),
            bootstrap_md: String::new(),
            memory_md: String::new(),
        }
    }

    #[test]
    fn assembled_system_prompt_combines_parts() {
        let persona = Persona {
            agents_md: "You are helpful.".into(),
            soul_md: "Be warm and friendly.".into(),
            user_md: "Name: Dragon".into(),
            identity_md: "Name: TestBot".into(),
            tools_md: "SSH: localhost".into(),
            ..make_persona()
        };

        let assembled = persona.assembled_system_prompt();
        assert!(assembled.contains("You are helpful."));
        assert!(assembled.contains("# Project Context"));
        assert!(assembled.contains("## SOUL.md"));
        assert!(assembled.contains("Be warm"));
        assert!(assembled.contains("## IDENTITY.md"));
        assert!(assembled.contains("## USER.md"));
        assert!(assembled.contains("## TOOLS.md"));
    }

    #[test]
    fn has_heartbeat_tasks_detects_content() {
        let persona = Persona {
            heartbeat_md: "# HEARTBEAT.md\n\n# Just comments".into(),
            ..make_persona()
        };
        assert!(!persona.has_heartbeat_tasks());

        let persona2 = Persona {
            heartbeat_md: "# HEARTBEAT.md\n- Check email".into(),
            ..persona.clone()
        };
        assert!(persona2.has_heartbeat_tasks());
    }

    #[test]
    fn assembled_system_prompt_has_project_context_section() {
        let persona = Persona {
            agents_md: "Core instructions.".into(),
            soul_md: "Warm persona.".into(),
            tools_md: "SSH info.".into(),
            ..make_persona()
        };

        let assembled = persona.assembled_system_prompt();
        assert!(assembled.contains("# Project Context"));
        assert!(assembled.contains("project context files have been loaded"));
        assert!(assembled.contains("## SOUL.md"));
        assert!(assembled.contains("## TOOLS.md"));
    }

    #[test]
    fn assembled_system_prompt_soul_embody_instruction() {
        let persona = Persona {
            soul_md: "Be playful.".into(),
            ..make_persona()
        };

        let assembled = persona.assembled_system_prompt();
        assert!(assembled.contains("embody its persona and tone"));
        assert!(assembled.contains("Avoid stiff, generic replies"));
    }

    #[test]
    fn assembled_system_prompt_no_soul_no_embody_instruction() {
        let persona = Persona {
            tools_md: "SSH info.".into(),
            ..make_persona()
        };

        let assembled = persona.assembled_system_prompt();
        assert!(assembled.contains("# Project Context"));
        assert!(!assembled.contains("embody its persona"));
    }

    #[test]
    fn assembled_system_prompt_includes_bootstrap() {
        let persona = Persona {
            bootstrap_md: "Welcome! First-run setup.".into(),
            ..make_persona()
        };

        let assembled = persona.assembled_system_prompt();
        assert!(assembled.contains("## BOOTSTRAP.md"));
        assert!(assembled.contains("First-run setup"));
    }

    #[test]
    fn assembled_system_prompt_includes_heartbeat() {
        let persona = Persona {
            heartbeat_md: "- Check email every hour".into(),
            ..make_persona()
        };

        let assembled = persona.assembled_system_prompt();
        assert!(assembled.contains("## HEARTBEAT.md"));
        assert!(assembled.contains("Check email every hour"));
    }

    #[test]
    fn assembled_system_prompt_minimal_excludes_heartbeat_bootstrap() {
        let persona = Persona {
            soul_md: "Be warm.".into(),
            tools_md: "SSH info.".into(),
            heartbeat_md: "- Check email".into(),
            bootstrap_md: "Welcome!".into(),
            ..make_persona()
        };

        let minimal = persona.assembled_system_prompt_minimal();
        assert!(minimal.contains("## SOUL.md"));
        assert!(minimal.contains("## TOOLS.md"));
        assert!(!minimal.contains("HEARTBEAT.md"));
        assert!(!minimal.contains("BOOTSTRAP.md"));

        // Full mode should include them
        let full = persona.assembled_system_prompt();
        assert!(full.contains("## HEARTBEAT.md"));
        assert!(full.contains("## BOOTSTRAP.md"));
    }

    #[test]
    fn load_persona_from_workspace_reads_bootstrap() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let prompts_dir = root.join("prompts");
        std::fs::create_dir_all(&prompts_dir).unwrap();

        std::fs::write(prompts_dir.join("BOOTSTRAP.md"), "First-run onboarding.").unwrap();

        let persona = load_persona_from_workspace(root, "test", "Test", None).unwrap();
        assert!(persona.bootstrap_md.contains("First-run onboarding"));
    }

    #[test]
    fn truncate_context_file_preserves_short_content() {
        let short = "Hello world";
        let result = truncate_context_file(short, "TEST.md", 100);
        assert_eq!(result, "Hello world");
    }

    #[test]
    fn truncate_context_file_truncates_long_content() {
        let long_content: String = "A".repeat(1000);
        let result = truncate_context_file(&long_content, "TEST.md", 100);

        // Should contain truncation marker
        assert!(result.contains("truncated, read TEST.md for full content"));
        // Should be shorter than original
        assert!(result.len() < long_content.len());
        // Head (70 chars) and tail (20 chars) should be present
        assert!(result.starts_with(&"A".repeat(70)));
        assert!(result.ends_with(&"A".repeat(20)));
    }

    #[test]
    fn assembled_system_prompt_empty_context_no_project_section() {
        let persona = Persona {
            agents_md: "Core instructions.".into(),
            ..make_persona()
        };

        let assembled = persona.assembled_system_prompt();
        assert!(assembled.contains("Core instructions."));
        assert!(!assembled.contains("# Project Context"));
    }

    #[test]
    fn truncate_context_file_handles_multibyte_unicode() {
        // 100 Chinese characters (3 bytes each in UTF-8)
        let content = "\u{4f60}".repeat(100);
        let result = truncate_context_file(&content, "TEST.md", 50);

        // Should not panic, should contain truncation marker
        assert!(result.contains("truncated"));
        assert!(!result.is_empty());
    }

    #[test]
    fn truncate_context_file_handles_emoji() {
        // Emoji are 4 bytes in UTF-8
        let content = "\u{1F600}".repeat(100);
        let result = truncate_context_file(&content, "TEST.md", 50);

        assert!(result.contains("truncated"));
        assert!(!result.is_empty());
    }

    #[test]
    fn clamp_to_budget_truncates_with_ellipsis() {
        let content = "Hello, world! This is a test.";
        let clamped = clamp_to_budget(content, 10);
        assert_eq!(clamped.chars().count(), 10);
        assert!(clamped.ends_with('\u{2026}'));
    }

    #[test]
    fn clamp_to_budget_preserves_short_content() {
        let content = "Short";
        let clamped = clamp_to_budget(content, 100);
        assert_eq!(clamped, "Short");
    }

    #[test]
    fn clamp_to_budget_handles_zero_budget() {
        let clamped = clamp_to_budget("Hello", 0);
        assert_eq!(clamped, "");
    }

    #[test]
    fn clamp_to_budget_handles_multibyte() {
        let content = "\u{4f60}".repeat(20);
        let clamped = clamp_to_budget(&content, 5);
        assert_eq!(clamped.chars().count(), 5);
        assert!(clamped.ends_with('\u{2026}'));
    }

    #[test]
    fn assembled_system_prompt_includes_memory() {
        let persona = Persona {
            memory_md: "User prefers dark mode.".into(),
            ..make_persona()
        };

        let assembled = persona.assembled_system_prompt();
        assert!(assembled.contains("## MEMORY.md"));
        assert!(assembled.contains("dark mode"));
    }

    #[test]
    fn assembled_system_prompt_minimal_excludes_memory() {
        let persona = Persona {
            soul_md: "Be warm.".into(),
            memory_md: "User prefers dark mode.".into(),
            ..make_persona()
        };

        let minimal = persona.assembled_system_prompt_minimal();
        assert!(minimal.contains("## SOUL.md"));
        assert!(!minimal.contains("## MEMORY.md"));
    }

    #[test]
    fn load_persona_from_workspace_reads_memory() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let prompts_dir = root.join("prompts");
        std::fs::create_dir_all(&prompts_dir).unwrap();

        // MEMORY.md lives at workspace root (same as consolidation write path)
        std::fs::write(root.join("MEMORY.md"), "Long-term knowledge.").unwrap();

        let persona = load_persona_from_workspace(root, "test", "Test", None).unwrap();
        assert!(persona.memory_md.contains("Long-term knowledge"));
    }

    #[test]
    fn load_persona_from_workspace_reads_lowercase_memory() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let prompts_dir = root.join("prompts");
        std::fs::create_dir_all(&prompts_dir).unwrap();

        // Fallback: memory.md at workspace root
        std::fs::write(root.join("memory.md"), "Lowercase memory file.").unwrap();

        let persona = load_persona_from_workspace(root, "test", "Test", None).unwrap();
        assert!(persona.memory_md.contains("Lowercase memory"));
    }

    #[test]
    fn agents_md_gets_truncation_protection() {
        let long_agents: String = "X".repeat(25_000);
        let persona = Persona {
            agents_md: long_agents,
            ..make_persona()
        };

        let assembled = persona.assembled_system_prompt();
        assert!(assembled.contains("truncated, read AGENTS.md for full content"));
    }

    #[test]
    fn heartbeat_mode_only_agents_and_heartbeat() {
        let persona = Persona {
            agents_md: "Core instructions.".into(),
            soul_md: "Be warm.".into(),
            tools_md: "SSH info.".into(),
            identity_md: "Name: Kuro".into(),
            user_md: "User info.".into(),
            heartbeat_md: "- Check email".into(),
            bootstrap_md: "Welcome!".into(),
            memory_md: "Long-term memory.".into(),
            ..make_persona()
        };

        let hb = persona.assembled_system_prompt_heartbeat();
        assert!(hb.contains("Core instructions."));
        assert!(hb.contains("## HEARTBEAT.md"));
        assert!(hb.contains("Check email"));
        assert!(!hb.contains("## SOUL.md"));
        assert!(!hb.contains("## TOOLS.md"));
        assert!(!hb.contains("## IDENTITY.md"));
        assert!(!hb.contains("## USER.md"));
        assert!(!hb.contains("## BOOTSTRAP.md"));
        assert!(!hb.contains("## MEMORY.md"));
    }

    #[test]
    fn scheduled_mode_excludes_heartbeat_and_bootstrap_includes_memory() {
        let persona = Persona {
            soul_md: "Be warm.".into(),
            tools_md: "SSH info.".into(),
            heartbeat_md: "- Check email".into(),
            bootstrap_md: "Welcome!".into(),
            memory_md: "Long-term memory.".into(),
            ..make_persona()
        };

        let sched = persona.assembled_system_prompt_scheduled();
        assert!(sched.contains("## SOUL.md"));
        assert!(sched.contains("## TOOLS.md"));
        assert!(sched.contains("## MEMORY.md"));
        assert!(!sched.contains("## HEARTBEAT.md"));
        assert!(!sched.contains("## BOOTSTRAP.md"));
    }

    #[test]
    fn heartbeat_response_rules_in_full_mode() {
        let persona = Persona {
            heartbeat_md: "- Check email".into(),
            ..make_persona()
        };

        let full = persona.assembled_system_prompt();
        assert!(full.contains("## Heartbeats"));
        assert!(full.contains("reply exactly: HEARTBEAT_OK"));
    }

    #[test]
    fn heartbeat_response_rules_in_heartbeat_mode() {
        let persona = Persona {
            heartbeat_md: "- Check email".into(),
            ..make_persona()
        };

        let hb = persona.assembled_system_prompt_heartbeat();
        assert!(hb.contains("## Heartbeats"));
        assert!(hb.contains("reply exactly: HEARTBEAT_OK"));
    }

    #[test]
    fn no_heartbeat_rules_when_heartbeat_empty() {
        let persona = Persona {
            heartbeat_md: "# HEARTBEAT.md\n# Just comments".into(),
            ..make_persona()
        };

        let full = persona.assembled_system_prompt();
        assert!(!full.contains("## Heartbeats"));
    }

    #[test]
    fn no_heartbeat_rules_in_minimal_mode() {
        let persona = Persona {
            heartbeat_md: "- Check email".into(),
            ..make_persona()
        };

        let minimal = persona.assembled_system_prompt_minimal();
        assert!(!minimal.contains("## Heartbeats"));
    }

    #[test]
    fn tools_disclaimer_present_when_tools_md_set() {
        let persona = Persona {
            tools_md: "SSH info.".into(),
            ..make_persona()
        };

        let assembled = persona.assembled_system_prompt();
        assert!(assembled.contains("does not control tool availability"));
    }

    #[test]
    fn tools_disclaimer_absent_when_no_tools_md() {
        let persona = Persona {
            soul_md: "Be warm.".into(),
            ..make_persona()
        };

        let assembled = persona.assembled_system_prompt();
        assert!(!assembled.contains("does not control tool availability"));
    }

    #[test]
    fn identity_parsing_overrides_yaml_name_and_emoji() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let prompts_dir = root.join("prompts");
        std::fs::create_dir_all(&prompts_dir).unwrap();

        std::fs::write(
            prompts_dir.join("IDENTITY.md"),
            "- **Name:** Kuro\n- **Emoji:** 🐾\n- **Creature:** shadow cat\n- **Vibe:** sharp\n",
        )
        .unwrap();

        let persona =
            load_persona_from_workspace(root, "test", "YamlFallbackName", Some("⚙️")).unwrap();

        assert_eq!(persona.name, "Kuro");
        assert_eq!(persona.emoji.as_deref(), Some("🐾"));
        assert_eq!(persona.creature.as_deref(), Some("shadow cat"));
        assert_eq!(persona.vibe.as_deref(), Some("sharp"));
    }

    #[test]
    fn identity_parsing_falls_back_to_yaml_when_not_filled() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let prompts_dir = root.join("prompts");
        std::fs::create_dir_all(&prompts_dir).unwrap();

        std::fs::write(
            prompts_dir.join("IDENTITY.md"),
            "- **Name:**\n  _(pick something you like)_\n- **Emoji:**\n  _(your signature)_\n",
        )
        .unwrap();

        let persona = load_persona_from_workspace(root, "test", "YamlName", Some("⚙️")).unwrap();

        assert_eq!(persona.name, "YamlName");
        assert_eq!(persona.emoji.as_deref(), Some("⚙️"));
        assert!(persona.creature.is_none());
    }
}
