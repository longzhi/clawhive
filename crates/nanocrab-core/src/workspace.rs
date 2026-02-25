use std::path::{Path, PathBuf};

use anyhow::Result;

/// A Workspace is an agent's home directory — the single root for all its
/// persistent state: memory files, session JSONL, search index, and custom prompts.
#[derive(Debug, Clone)]
pub struct Workspace {
    root: PathBuf,
}

impl Workspace {
    pub fn new(root: impl AsRef<Path>) -> Self {
        Self {
            root: root.as_ref().to_path_buf(),
        }
    }

    /// Resolve workspace path from config.
    ///
    /// - Absolute path → use as-is
    /// - Relative path → resolve against project root
    /// - None → default to `<project_root>/workspaces/<agent_id>`
    pub fn resolve(
        project_root: &Path,
        agent_id: &str,
        configured: Option<&str>,
    ) -> Self {
        let root = match configured {
            Some(path) => {
                let p = PathBuf::from(path);
                if p.is_absolute() {
                    p
                } else {
                    project_root.join(p)
                }
            }
            None => project_root.join("workspaces").join(agent_id),
        };
        Self { root }
    }

    /// The workspace root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Directory for daily memory files (`memory/`).
    pub fn memory_dir(&self) -> PathBuf {
        self.root.join("memory")
    }

    /// Directory for session JSONL files (`sessions/`).
    pub fn sessions_dir(&self) -> PathBuf {
        self.root.join("sessions")
    }

    /// Path to the long-term memory file (`MEMORY.md`).
    pub fn long_term_memory(&self) -> PathBuf {
        self.root.join("MEMORY.md")
    }

    /// Path to the SQLite search index (`index.db`).
    pub fn index_db_path(&self) -> PathBuf {
        self.root.join("index.db")
    }

    /// Directory for prompt/persona files (`prompts/`).
    pub fn prompts_dir(&self) -> PathBuf {
        self.root.join("prompts")
    }

    /// Path to AGENTS.md (operating instructions).
    pub fn agents_md(&self) -> PathBuf {
        self.prompts_dir().join("AGENTS.md")
    }

    /// Path to SOUL.md (personality/vibe).
    pub fn soul_md(&self) -> PathBuf {
        self.prompts_dir().join("SOUL.md")
    }

    /// Path to USER.md (user information).
    pub fn user_md(&self) -> PathBuf {
        self.prompts_dir().join("USER.md")
    }

    /// Path to IDENTITY.md (agent identity).
    pub fn identity_md(&self) -> PathBuf {
        self.prompts_dir().join("IDENTITY.md")
    }

    /// Path to TOOLS.md (environment notes).
    pub fn tools_md(&self) -> PathBuf {
        self.prompts_dir().join("TOOLS.md")
    }

    /// Path to HEARTBEAT.md (periodic tasks).
    pub fn heartbeat_md(&self) -> PathBuf {
        self.prompts_dir().join("HEARTBEAT.md")
    }

    /// Path to BOOTSTRAP.md (first-run guide, deleted after setup).
    pub fn bootstrap_md(&self) -> PathBuf {
        self.prompts_dir().join("BOOTSTRAP.md")
    }

    /// Create required directories if they don't exist.
    pub async fn ensure_dirs(&self) -> Result<()> {
        tokio::fs::create_dir_all(&self.root).await?;
        tokio::fs::create_dir_all(self.memory_dir()).await?;
        tokio::fs::create_dir_all(self.sessions_dir()).await?;
        tokio::fs::create_dir_all(self.prompts_dir()).await?;
        Ok(())
    }

    /// Initialize workspace with default prompt files if they don't exist.
    /// Returns true if this is a new workspace (BOOTSTRAP.md was created).
    pub async fn init_with_defaults(&self) -> Result<bool> {
        use crate::templates;
        
        self.ensure_dirs().await?;
        
        let mut is_new = false;
        
        // Create prompt files if they don't exist
        let files = [
            (self.agents_md(), templates::AGENTS_MD),
            (self.soul_md(), templates::SOUL_MD),
            (self.user_md(), templates::USER_MD),
            (self.identity_md(), templates::IDENTITY_MD),
            (self.tools_md(), templates::TOOLS_MD),
            (self.heartbeat_md(), templates::HEARTBEAT_MD),
        ];
        
        for (path, content) in files {
            if !path.exists() {
                tokio::fs::write(&path, content).await?;
            }
        }
        
        // BOOTSTRAP.md is special - only create for truly new workspaces
        // and it should be deleted after first run
        let bootstrap_path = self.bootstrap_md();
        if !bootstrap_path.exists() && !self.long_term_memory().exists() {
            // No BOOTSTRAP.md and no MEMORY.md = new workspace
            tokio::fs::write(&bootstrap_path, templates::BOOTSTRAP_MD).await?;
            is_new = true;
        }
        
        Ok(is_new)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_with_absolute_path() {
        let ws = Workspace::resolve(
            Path::new("/project"),
            "main",
            Some("/custom/workspace"),
        );
        assert_eq!(ws.root(), Path::new("/custom/workspace"));
    }

    #[test]
    fn resolve_with_relative_path() {
        let ws = Workspace::resolve(
            Path::new("/project"),
            "main",
            Some("./workspaces/custom"),
        );
        assert_eq!(ws.root(), Path::new("/project/./workspaces/custom"));
    }

    #[test]
    fn resolve_with_none_uses_default() {
        let ws = Workspace::resolve(Path::new("/project"), "agent-1", None);
        assert_eq!(ws.root(), Path::new("/project/workspaces/agent-1"));
    }

    #[test]
    fn path_derivations() {
        let ws = Workspace::new("/home/agent");
        assert_eq!(ws.memory_dir(), PathBuf::from("/home/agent/memory"));
        assert_eq!(ws.sessions_dir(), PathBuf::from("/home/agent/sessions"));
        assert_eq!(ws.long_term_memory(), PathBuf::from("/home/agent/MEMORY.md"));
        assert_eq!(ws.index_db_path(), PathBuf::from("/home/agent/index.db"));
        assert_eq!(ws.prompts_dir(), PathBuf::from("/home/agent/prompts"));
        assert_eq!(ws.agents_md(), PathBuf::from("/home/agent/prompts/AGENTS.md"));
        assert_eq!(ws.soul_md(), PathBuf::from("/home/agent/prompts/SOUL.md"));
        assert_eq!(ws.heartbeat_md(), PathBuf::from("/home/agent/prompts/HEARTBEAT.md"));
    }

    #[tokio::test]
    async fn ensure_dirs_creates_directories() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let ws = Workspace::new(tmp.path().join("ws"));
        ws.ensure_dirs().await.expect("ensure_dirs");

        assert!(ws.root().exists());
        assert!(ws.memory_dir().exists());
        assert!(ws.sessions_dir().exists());
    }
}
