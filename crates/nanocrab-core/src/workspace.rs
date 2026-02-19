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

    /// Directory for custom prompt overrides (`prompts/`).
    pub fn prompts_dir(&self) -> PathBuf {
        self.root.join("prompts")
    }

    /// Create required directories if they don't exist.
    pub async fn ensure_dirs(&self) -> Result<()> {
        tokio::fs::create_dir_all(&self.root).await?;
        tokio::fs::create_dir_all(self.memory_dir()).await?;
        tokio::fs::create_dir_all(self.sessions_dir()).await?;
        Ok(())
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
