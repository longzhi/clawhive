//! Peer registry for multi-agent collaboration.
//! Provides auto-discovery of agents and their identities.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Information about a peer agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerInfo {
    /// Agent ID (directory name)
    pub agent_id: String,
    /// Display name from IDENTITY.md
    pub name: String,
    /// Role (e.g., "Code Engineer", "Creative", "Researcher")
    pub role: Option<String>,
    /// What they're good at
    pub specialization: Option<String>,
    /// Personality vibe
    pub vibe: Option<String>,
    /// Signature emoji
    pub emoji: Option<String>,
    /// Workspace path
    pub workspace_path: PathBuf,
}

/// Registry of all known agents in the system.
#[derive(Debug, Clone, Default)]
pub struct PeerRegistry {
    peers: HashMap<String, PeerInfo>,
}

impl PeerRegistry {
    /// Create a new empty registry.
    pub fn new() -> Self {
        Self {
            peers: HashMap::new(),
        }
    }

    /// Scan workspaces directory and auto-discover agents.
    /// Each subdirectory with prompts/IDENTITY.md is considered an agent.
    pub fn scan_workspaces(workspaces_root: &Path) -> Result<Self> {
        let mut registry = Self::new();

        if !workspaces_root.exists() {
            return Ok(registry);
        }

        for entry in std::fs::read_dir(workspaces_root)
            .with_context(|| format!("reading workspaces dir: {}", workspaces_root.display()))?
        {
            let entry = entry?;
            let path = entry.path();

            if !path.is_dir() {
                continue;
            }

            let agent_id = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .to_string();

            // Check if this looks like a workspace (has prompts/IDENTITY.md)
            let identity_path = path.join("prompts").join("IDENTITY.md");
            if identity_path.exists() {
                if let Ok(peer) = load_peer_from_workspace(&path, &agent_id) {
                    registry.peers.insert(agent_id, peer);
                }
            }
        }

        Ok(registry)
    }

    /// Register a peer manually (e.g., from config).
    pub fn register(&mut self, peer: PeerInfo) {
        self.peers.insert(peer.agent_id.clone(), peer);
    }

    /// Get a peer by agent_id.
    pub fn get(&self, agent_id: &str) -> Option<&PeerInfo> {
        self.peers.get(agent_id)
    }

    /// Get all peers.
    pub fn all(&self) -> impl Iterator<Item = &PeerInfo> {
        self.peers.values()
    }

    /// Get all peers except the specified agent.
    pub fn peers_except(&self, exclude_agent_id: &str) -> Vec<&PeerInfo> {
        self.peers
            .values()
            .filter(|p| p.agent_id != exclude_agent_id)
            .collect()
    }

    /// Format peers as markdown for prompt injection.
    /// Excludes the current agent.
    pub fn format_peers_md(&self, exclude_agent_id: &str) -> String {
        let peers: Vec<_> = self.peers_except(exclude_agent_id);

        if peers.is_empty() {
            return String::new();
        }

        let mut lines = vec!["## 你的同事 / Your Peers".to_string()];
        lines.push(String::new());
        lines.push("当需要协作时，在群里 @ 对应的同事。".to_string());
        lines.push("When you need collaboration, @ the relevant peer in the chat.".to_string());
        lines.push(String::new());

        for peer in peers {
            let emoji = peer.emoji.as_deref().unwrap_or("🤖");
            let name = &peer.name;
            let role = peer.role.as_deref().unwrap_or("Agent");

            let mut desc = format!("- **{emoji} {name}** ({role})");

            if let Some(spec) = &peer.specialization {
                desc.push_str(&format!(": {spec}"));
            }

            if let Some(vibe) = &peer.vibe {
                desc.push_str(&format!(" — {vibe}"));
            }

            lines.push(desc);
        }

        lines.join("\n")
    }

    /// Number of registered peers.
    pub fn len(&self) -> usize {
        self.peers.len()
    }

    /// Check if registry is empty.
    pub fn is_empty(&self) -> bool {
        self.peers.is_empty()
    }
}

/// Load peer info from a workspace directory by parsing IDENTITY.md.
fn load_peer_from_workspace(workspace_path: &Path, agent_id: &str) -> Result<PeerInfo> {
    let identity_path = workspace_path.join("prompts").join("IDENTITY.md");
    let content = std::fs::read_to_string(&identity_path)
        .with_context(|| format!("reading IDENTITY.md at {}", identity_path.display()))?;

    let parsed = parse_identity_md(&content);

    Ok(PeerInfo {
        agent_id: agent_id.to_string(),
        name: parsed.name.unwrap_or_else(|| agent_id.to_string()),
        role: parsed.role,
        specialization: parsed.specialization,
        vibe: parsed.vibe,
        emoji: parsed.emoji,
        workspace_path: workspace_path.to_path_buf(),
    })
}

/// Parsed fields from IDENTITY.md
#[derive(Debug, Default)]
struct ParsedIdentity {
    name: Option<String>,
    role: Option<String>,
    specialization: Option<String>,
    vibe: Option<String>,
    emoji: Option<String>,
}

/// Simple parser for IDENTITY.md format.
/// Looks for lines like "- **Name:** value" or "- **Role:** value"
fn parse_identity_md(content: &str) -> ParsedIdentity {
    let mut parsed = ParsedIdentity::default();

    for line in content.lines() {
        let line = line.trim();

        // Match patterns like "- **Name:** value" or "**Name:** value"
        if let Some(value) = extract_field(line, "Name") {
            parsed.name = Some(value);
        } else if let Some(value) = extract_field(line, "Role") {
            parsed.role = Some(value);
        } else if let Some(value) = extract_field(line, "Specialization") {
            parsed.specialization = Some(value);
        } else if let Some(value) = extract_field(line, "Vibe") {
            parsed.vibe = Some(value);
        } else if let Some(value) = extract_field(line, "Emoji") {
            parsed.emoji = Some(value);
        }
    }

    parsed
}

/// Extract a field value from a line.
/// Matches patterns like:
/// - "- **Name:** value"
/// - "**Name:** value"
/// - "Name: value"
fn extract_field(line: &str, field_name: &str) -> Option<String> {
    // Remove leading "- " if present
    let line = line.strip_prefix('-').map(|s| s.trim()).unwrap_or(line);

    // Try "**Field:** value" pattern
    let bold_pattern = format!("**{}:**", field_name);
    if let Some(rest) = line.strip_prefix(&bold_pattern) {
        let value = rest.trim();
        if !value.is_empty() && !value.starts_with('_') {
            return Some(value.to_string());
        }
    }

    // Try "Field:" pattern (case-insensitive)
    let simple_pattern = format!("{}:", field_name);
    if line
        .to_lowercase()
        .starts_with(&simple_pattern.to_lowercase())
    {
        let value = line[simple_pattern.len()..].trim();
        if !value.is_empty() && !value.starts_with('_') {
            return Some(value.to_string());
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn parse_identity_md_extracts_fields() {
        let content = r#"
# IDENTITY.md

- **Name:** 小螃蟹1号
- **Role:** Code Engineer
- **Specialization:** Rust, 系统编程, 代码审查
- **Vibe:** 严谨务实
- **Emoji:** 🦀
"#;
        let parsed = parse_identity_md(content);
        assert_eq!(parsed.name, Some("小螃蟹1号".to_string()));
        assert_eq!(parsed.role, Some("Code Engineer".to_string()));
        assert_eq!(
            parsed.specialization,
            Some("Rust, 系统编程, 代码审查".to_string())
        );
        assert_eq!(parsed.vibe, Some("严谨务实".to_string()));
        assert_eq!(parsed.emoji, Some("🦀".to_string()));
    }

    #[test]
    fn parse_identity_md_ignores_placeholder_values() {
        let content = r#"
- **Name:** _(pick something you like)_
- **Role:** _(General Assistant?)_
"#;
        let parsed = parse_identity_md(content);
        assert_eq!(parsed.name, None);
        assert_eq!(parsed.role, None);
    }

    #[test]
    fn scan_workspaces_discovers_agents() {
        let tmp = TempDir::new().unwrap();
        let workspaces = tmp.path();

        // Create agent 1
        let agent1_dir = workspaces.join("crab-1").join("prompts");
        std::fs::create_dir_all(&agent1_dir).unwrap();
        std::fs::write(
            agent1_dir.join("IDENTITY.md"),
            "- **Name:** 小螃蟹1号\n- **Role:** Code Engineer\n- **Emoji:** 🦀",
        )
        .unwrap();

        // Create agent 2
        let agent2_dir = workspaces.join("crab-2").join("prompts");
        std::fs::create_dir_all(&agent2_dir).unwrap();
        std::fs::write(
            agent2_dir.join("IDENTITY.md"),
            "- **Name:** 小螃蟹2号\n- **Role:** Creative\n- **Emoji:** 🎨",
        )
        .unwrap();

        let registry = PeerRegistry::scan_workspaces(workspaces).unwrap();
        assert_eq!(registry.len(), 2);

        let crab1 = registry.get("crab-1").unwrap();
        assert_eq!(crab1.name, "小螃蟹1号");
        assert_eq!(crab1.role, Some("Code Engineer".to_string()));

        let crab2 = registry.get("crab-2").unwrap();
        assert_eq!(crab2.name, "小螃蟹2号");
        assert_eq!(crab2.role, Some("Creative".to_string()));
    }

    #[test]
    fn format_peers_md_excludes_self() {
        let mut registry = PeerRegistry::new();
        registry.register(PeerInfo {
            agent_id: "crab-1".to_string(),
            name: "小螃蟹1号".to_string(),
            role: Some("Code Engineer".to_string()),
            specialization: Some("Rust, 系统编程".to_string()),
            vibe: Some("严谨".to_string()),
            emoji: Some("🦀".to_string()),
            workspace_path: PathBuf::from("/tmp/crab-1"),
        });
        registry.register(PeerInfo {
            agent_id: "crab-2".to_string(),
            name: "小螃蟹2号".to_string(),
            role: Some("Creative".to_string()),
            specialization: None,
            vibe: None,
            emoji: Some("🎨".to_string()),
            workspace_path: PathBuf::from("/tmp/crab-2"),
        });

        let md = registry.format_peers_md("crab-1");
        assert!(md.contains("小螃蟹2号"));
        assert!(!md.contains("小螃蟹1号")); // Should exclude self
    }

    #[test]
    fn empty_registry_returns_empty_string() {
        let registry = PeerRegistry::new();
        let md = registry.format_peers_md("any");
        assert!(md.is_empty());
    }
}
