use anyhow::{Context, Result};
use std::path::Path;

/// Persona holds all workspace context files that define an agent's identity and behavior.
/// Follows OpenClaw-style workspace structure.
#[derive(Debug, Clone)]
pub struct Persona {
    pub agent_id: String,
    pub name: String,
    pub emoji: Option<String>,
    /// AGENTS.md - Operating instructions, memory rules, behavior guidelines
    pub agents_md: String,
    /// SOUL.md - Personality, vibe, boundaries
    pub soul_md: String,
    /// USER.md - Information about the human user
    pub user_md: String,
    /// IDENTITY.md - Agent's own identity (name, creature, emoji)
    pub identity_md: String,
    /// TOOLS.md - Local environment notes (SSH hosts, device names, etc.)
    pub tools_md: String,
    /// HEARTBEAT.md - Periodic task checklist
    pub heartbeat_md: String,
    /// Peers context - information about other agents (auto-generated)
    pub peers_context: String,
    /// Group members context - who's in the current chat (injected per-message)
    pub group_members_context: String,
}

impl Persona {
    /// Assembles all context files into a single system prompt.
    /// Order: AGENTS.md (core) -> SOUL.md (personality) -> TOOLS.md (environment)
    /// USER.md and IDENTITY.md are injected as context sections.
    /// Peers and group members context are added for multi-agent collaboration.
    pub fn assembled_system_prompt(&self) -> String {
        let mut parts = Vec::new();

        // Core operating instructions
        if !self.agents_md.is_empty() {
            parts.push(self.agents_md.clone());
        }

        // Personality and boundaries
        if !self.soul_md.is_empty() {
            parts.push(format!("\n## Soul\n{}", self.soul_md));
        }

        // Agent identity context
        if !self.identity_md.is_empty() {
            parts.push(format!("\n## Identity\n{}", self.identity_md));
        }

        // User context
        if !self.user_md.is_empty() {
            parts.push(format!("\n## User\n{}", self.user_md));
        }

        // Environment notes
        if !self.tools_md.is_empty() {
            parts.push(format!("\n## Tools\n{}", self.tools_md));
        }

        // Peer agents (for multi-agent collaboration)
        if !self.peers_context.is_empty() {
            parts.push(format!("\n{}", self.peers_context));
        }

        // Group members (injected per-message for group chats)
        if !self.group_members_context.is_empty() {
            parts.push(format!("\n{}", self.group_members_context));
        }

        parts.join("\n")
    }

    /// Set peers context (usually from PeerRegistry).
    pub fn with_peers_context(mut self, context: String) -> Self {
        self.peers_context = context;
        self
    }

    /// Set group members context (for current chat).
    pub fn with_group_members_context(mut self, context: String) -> Self {
        self.group_members_context = context;
        self
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

/// Load persona from workspace's prompts directory (OpenClaw-style).
/// Reads: {workspace}/prompts/AGENTS.md, SOUL.md, USER.md, IDENTITY.md, TOOLS.md, HEARTBEAT.md
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

    Ok(Persona {
        agent_id: agent_id.to_string(),
        name: name.to_string(),
        emoji: emoji.map(|s| s.to_string()),
        agents_md,
        soul_md,
        user_md,
        identity_md,
        tools_md,
        heartbeat_md,
        peers_context: String::new(),
        group_members_context: String::new(),
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
        agents_md,
        soul_md: String::new(),
        user_md: String::new(),
        identity_md: String::new(),
        tools_md: String::new(),
        heartbeat_md: String::new(),
        peers_context: String::new(),
        group_members_context: String::new(),
    })
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

    #[test]
    fn assembled_system_prompt_combines_parts() {
        let persona = Persona {
            agent_id: "test".into(),
            name: "Test".into(),
            emoji: None,
            agents_md: "You are helpful.".into(),
            soul_md: "Be warm and friendly.".into(),
            user_md: "Name: Dragon".into(),
            identity_md: "Name: TestBot".into(),
            tools_md: "SSH: localhost".into(),
            heartbeat_md: String::new(),
            peers_context: String::new(),
            group_members_context: String::new(),
        };

        let assembled = persona.assembled_system_prompt();
        assert!(assembled.contains("You are helpful."));
        assert!(assembled.contains("## Soul"));
        assert!(assembled.contains("Be warm"));
        assert!(assembled.contains("## Identity"));
        assert!(assembled.contains("## User"));
        assert!(assembled.contains("## Tools"));
    }

    #[test]
    fn has_heartbeat_tasks_detects_content() {
        let persona = Persona {
            agent_id: "test".into(),
            name: "Test".into(),
            emoji: None,
            agents_md: String::new(),
            soul_md: String::new(),
            user_md: String::new(),
            identity_md: String::new(),
            tools_md: String::new(),
            heartbeat_md: "# HEARTBEAT.md\n\n# Just comments".into(),
            peers_context: String::new(),
            group_members_context: String::new(),
        };
        assert!(!persona.has_heartbeat_tasks());

        let persona2 = Persona {
            heartbeat_md: "# HEARTBEAT.md\n- Check email".into(),
            ..persona.clone()
        };
        assert!(persona2.has_heartbeat_tasks());
    }

    #[test]
    fn assembled_system_prompt_includes_peers() {
        let persona = Persona {
            agent_id: "test".into(),
            name: "Test".into(),
            emoji: None,
            agents_md: "You are helpful.".into(),
            soul_md: String::new(),
            user_md: String::new(),
            identity_md: String::new(),
            tools_md: String::new(),
            heartbeat_md: String::new(),
            peers_context: "## 你的同事\n- **🦀 小螃蟹1号** (Code Engineer)".into(),
            group_members_context: String::new(),
        };

        let assembled = persona.assembled_system_prompt();
        assert!(assembled.contains("你的同事"));
        assert!(assembled.contains("小螃蟹1号"));
    }
}
