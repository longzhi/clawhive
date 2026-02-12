use anyhow::{Context, Result};
use std::path::Path;

#[derive(Debug, Clone)]
pub struct Persona {
    pub agent_id: String,
    pub name: String,
    pub emoji: Option<String>,
    pub system_prompt: String,
    pub style_prompt: String,
    pub safety_prompt: String,
}

impl Persona {
    /// Assembles system + style + safety into a single system prompt
    pub fn assembled_system_prompt(&self) -> String {
        let mut parts = Vec::new();
        parts.push(self.system_prompt.clone());
        if !self.style_prompt.is_empty() {
            parts.push(format!("\n## Style\n{}", self.style_prompt));
        }
        if !self.safety_prompt.is_empty() {
            parts.push(format!("\n## Safety\n{}", self.safety_prompt));
        }
        parts.join("\n")
    }
}

/// Load persona from prompts directory
/// Reads: prompts/{agent_id}/system.md, style.md, safety.md
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

    Ok(Persona {
        agent_id: agent_id.to_string(),
        name: name.to_string(),
        emoji: emoji.map(|s| s.to_string()),
        system_prompt,
        style_prompt,
        safety_prompt,
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
    use std::path::PathBuf;

    fn prompts_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("prompts")
    }

    #[test]
    fn load_persona_reads_existing_prompts() {
        let persona =
            load_persona(&prompts_root(), "nanocrab-main", "nanocrab", Some("🦀")).unwrap();

        assert_eq!(persona.agent_id, "nanocrab-main");
        assert_eq!(persona.name, "nanocrab");
        assert_eq!(persona.emoji, Some("🦀".to_string()));
        assert!(!persona.system_prompt.is_empty());
    }

    #[test]
    fn load_persona_missing_files_fallback_empty() {
        let persona = load_persona(&prompts_root(), "nonexistent-agent", "test", None).unwrap();

        assert!(persona.system_prompt.is_empty());
        assert!(persona.style_prompt.is_empty());
        assert!(persona.safety_prompt.is_empty());
    }

    #[test]
    fn assembled_system_prompt_combines_parts() {
        let persona = Persona {
            agent_id: "test".into(),
            name: "Test".into(),
            emoji: None,
            system_prompt: "You are helpful.".into(),
            style_prompt: "Be concise.".into(),
            safety_prompt: "Be safe.".into(),
        };

        let assembled = persona.assembled_system_prompt();
        assert!(assembled.contains("You are helpful."));
        assert!(assembled.contains("## Style"));
        assert!(assembled.contains("Be concise."));
        assert!(assembled.contains("## Safety"));
        assert!(assembled.contains("Be safe."));
    }
}
