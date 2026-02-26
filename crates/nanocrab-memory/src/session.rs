use std::path::{Path, PathBuf};

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tracing::warn;
use uuid::Uuid;

/// JSONL line types for session recording
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEntry {
    Session {
        version: u32,
        id: String,
        timestamp: DateTime<Utc>,
        agent_id: String,
    },
    Message {
        id: String,
        timestamp: DateTime<Utc>,
        message: SessionMessage,
    },
    ToolCall {
        id: String,
        timestamp: DateTime<Utc>,
        tool: String,
        input: serde_json::Value,
    },
    ToolResult {
        id: String,
        timestamp: DateTime<Utc>,
        tool: String,
        output: serde_json::Value,
    },
    Compaction {
        id: String,
        timestamp: DateTime<Utc>,
        summary: String,
        dropped_before: String,
    },
    ModelChange {
        id: String,
        timestamp: DateTime<Utc>,
        model: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionMessage {
    pub role: String,
    pub content: String,
}

/// Append-only writer for session JSONL files
pub struct SessionWriter {
    sessions_dir: PathBuf,
}

impl SessionWriter {
    pub fn new(workspace: impl AsRef<Path>) -> Self {
        Self {
            sessions_dir: workspace.as_ref().join("sessions"),
        }
    }

    /// Start a new session file. Writes the header line. Returns the session file path.
    pub async fn start_session(&self, session_id: &str, agent_id: &str) -> Result<PathBuf> {
        tokio::fs::create_dir_all(&self.sessions_dir).await?;
        let path = self.session_path(session_id);
        let entry = SessionEntry::Session {
            version: 1,
            id: session_id.to_owned(),
            timestamp: Utc::now(),
            agent_id: agent_id.to_owned(),
        };
        let line = serde_json::to_string(&entry)?;
        tokio::fs::write(&path, format!("{line}\n")).await?;
        Ok(path)
    }

    /// Append a single entry to an existing session file
    pub async fn append(&self, session_id: &str, entry: SessionEntry) -> Result<()> {
        tokio::fs::create_dir_all(&self.sessions_dir).await?;
        let path = self.session_path(session_id);
        let mut file = tokio::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(path)
            .await?;
        let line = serde_json::to_string(&entry)?;
        file.write_all(line.as_bytes()).await?;
        file.write_all(b"\n").await?;
        Ok(())
    }

    /// Convenience: append a user or assistant message
    pub async fn append_message(&self, session_id: &str, role: &str, content: &str) -> Result<()> {
        let entry = SessionEntry::Message {
            id: Uuid::new_v4().to_string(),
            timestamp: Utc::now(),
            message: SessionMessage {
                role: role.to_owned(),
                content: content.to_owned(),
            },
        };
        self.append(session_id, entry).await
    }

    /// Helper to get session file path
    fn session_path(&self, session_id: &str) -> PathBuf {
        self.sessions_dir.join(format!("{session_id}.jsonl"))
    }

    /// Clear (delete) a session file. Returns true if the file was deleted.
    pub async fn clear_session(&self, session_id: &str) -> Result<bool> {
        let path = self.session_path(session_id);
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e.into()),
        }
    }
}

/// Reader for session JSONL files
pub struct SessionReader {
    sessions_dir: PathBuf,
}

impl SessionReader {
    pub fn new(workspace: impl AsRef<Path>) -> Self {
        Self {
            sessions_dir: workspace.as_ref().join("sessions"),
        }
    }

    /// Load the last N message entries from a session (type=message only, ordered chronologically)
    pub async fn load_recent_messages(
        &self,
        session_id: &str,
        limit: usize,
    ) -> Result<Vec<SessionMessage>> {
        if limit == 0 {
            return Ok(Vec::new());
        }

        let entries = self.load_all_entries(session_id).await?;
        let mut messages: Vec<SessionMessage> = entries
            .into_iter()
            .filter_map(|entry| match entry {
                SessionEntry::Message { message, .. } => Some(message),
                _ => None,
            })
            .collect();

        if messages.len() > limit {
            let start = messages.len() - limit;
            messages = messages.split_off(start);
        }

        Ok(messages)
    }

    /// Load ALL entries from a session file
    pub async fn load_all_entries(&self, session_id: &str) -> Result<Vec<SessionEntry>> {
        let path = self.sessions_dir.join(format!("{session_id}.jsonl"));
        let content = tokio::fs::read_to_string(path).await?;
        let mut entries = Vec::new();

        for (index, line) in content.lines().enumerate() {
            match serde_json::from_str::<SessionEntry>(line) {
                Ok(entry) => entries.push(entry),
                Err(error) => warn!(line = index + 1, %error, "failed to parse session entry line"),
            }
        }

        Ok(entries)
    }

    /// Check if a session file exists
    pub async fn session_exists(&self, session_id: &str) -> bool {
        tokio::fs::metadata(self.sessions_dir.join(format!("{session_id}.jsonl")))
            .await
            .is_ok()
    }

    /// List all session IDs (from filenames), sorted by modification time (newest first)
    pub async fn list_sessions(&self) -> Result<Vec<String>> {
        let mut items: Vec<(String, std::time::SystemTime)> = Vec::new();
        let mut dir = match tokio::fs::read_dir(&self.sessions_dir).await {
            Ok(dir) => dir,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };

        while let Some(entry) = dir.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                continue;
            }

            let Some(stem) = path.file_stem().and_then(|name| name.to_str()) else {
                continue;
            };

            let modified = entry
                .metadata()
                .await
                .and_then(|meta| meta.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            items.push((stem.to_owned(), modified));
        }

        items.sort_by(|a, b| b.1.cmp(&a.1));
        Ok(items.into_iter().map(|(id, _)| id).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    async fn read_lines(path: &Path) -> Vec<String> {
        let raw = tokio::fs::read_to_string(path)
            .await
            .expect("read session file");
        raw.lines().map(ToOwned::to_owned).collect()
    }

    #[tokio::test]
    async fn test_start_session_creates_file() {
        let tmp = TempDir::new().expect("tempdir");
        let writer = SessionWriter::new(tmp.path());

        let path = writer
            .start_session("s1", "main")
            .await
            .expect("start session");
        assert!(path.exists());

        let lines = read_lines(&path).await;
        assert_eq!(lines.len(), 1);

        let value: serde_json::Value = serde_json::from_str(&lines[0]).expect("valid json");
        assert_eq!(value["type"], "session");
        assert_eq!(value["version"], 1);
        assert_eq!(value["id"], "s1");
        assert_eq!(value["agent_id"], "main");
        assert!(value["timestamp"].as_str().is_some());
    }

    #[tokio::test]
    async fn test_append_message() {
        let tmp = TempDir::new().expect("tempdir");
        let writer = SessionWriter::new(tmp.path());
        writer
            .start_session("s1", "main")
            .await
            .expect("start session");

        writer
            .append_message("s1", "user", "hello")
            .await
            .expect("append user");
        writer
            .append_message("s1", "assistant", "hi!")
            .await
            .expect("append assistant");

        let path = tmp.path().join("sessions/s1.jsonl");
        let lines = read_lines(&path).await;
        assert_eq!(lines.len(), 3);

        let msg1: serde_json::Value = serde_json::from_str(&lines[1]).expect("json msg1");
        let msg2: serde_json::Value = serde_json::from_str(&lines[2]).expect("json msg2");
        assert_eq!(msg1["type"], "message");
        assert_eq!(msg1["message"]["role"], "user");
        assert_eq!(msg1["message"]["content"], "hello");
        assert_eq!(msg2["type"], "message");
        assert_eq!(msg2["message"]["role"], "assistant");
        assert_eq!(msg2["message"]["content"], "hi!");
    }

    #[tokio::test]
    async fn test_load_recent_messages() {
        let tmp = TempDir::new().expect("tempdir");
        let writer = SessionWriter::new(tmp.path());
        let reader = SessionReader::new(tmp.path());
        writer
            .start_session("s1", "main")
            .await
            .expect("start session");

        writer.append_message("s1", "user", "m1").await.expect("m1");
        writer
            .append_message("s1", "assistant", "m2")
            .await
            .expect("m2");
        writer.append_message("s1", "user", "m3").await.expect("m3");

        let messages = reader
            .load_recent_messages("s1", 2)
            .await
            .expect("load recent");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].content, "m2");
        assert_eq!(messages[1].content, "m3");
    }

    #[tokio::test]
    async fn test_load_recent_messages_limit() {
        let tmp = TempDir::new().expect("tempdir");
        let writer = SessionWriter::new(tmp.path());
        let reader = SessionReader::new(tmp.path());
        writer
            .start_session("s1", "main")
            .await
            .expect("start session");

        for i in 0..5 {
            writer
                .append_message("s1", "user", &format!("msg-{i}"))
                .await
                .expect("append");
        }

        let messages = reader
            .load_recent_messages("s1", 3)
            .await
            .expect("load recent");
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].content, "msg-2");
        assert_eq!(messages[1].content, "msg-3");
        assert_eq!(messages[2].content, "msg-4");
    }

    #[tokio::test]
    async fn test_load_all_entries() {
        let tmp = TempDir::new().expect("tempdir");
        let writer = SessionWriter::new(tmp.path());
        let reader = SessionReader::new(tmp.path());
        writer
            .start_session("s1", "main")
            .await
            .expect("start session");

        writer
            .append_message("s1", "user", "hello")
            .await
            .expect("append message");
        writer
            .append(
                "s1",
                SessionEntry::ModelChange {
                    id: "mc1".to_owned(),
                    timestamp: Utc::now(),
                    model: "gpt-x".to_owned(),
                },
            )
            .await
            .expect("append model change");

        let entries = reader.load_all_entries("s1").await.expect("load all");
        assert_eq!(entries.len(), 3);
    }

    #[tokio::test]
    async fn test_session_exists() {
        let tmp = TempDir::new().expect("tempdir");
        let writer = SessionWriter::new(tmp.path());
        let reader = SessionReader::new(tmp.path());

        assert!(!reader.session_exists("missing").await);
        writer
            .start_session("s1", "main")
            .await
            .expect("start session");
        assert!(reader.session_exists("s1").await);
    }

    #[tokio::test]
    async fn test_list_sessions() {
        let tmp = TempDir::new().expect("tempdir");
        let writer = SessionWriter::new(tmp.path());
        let reader = SessionReader::new(tmp.path());

        writer.start_session("s1", "main").await.expect("s1");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        writer.start_session("s2", "main").await.expect("s2");

        let sessions = reader.list_sessions().await.expect("list");
        assert_eq!(sessions, vec!["s2".to_owned(), "s1".to_owned()]);
    }

    #[tokio::test]
    async fn test_append_to_nonexistent_session() {
        let tmp = TempDir::new().expect("tempdir");
        let writer = SessionWriter::new(tmp.path());
        let reader = SessionReader::new(tmp.path());

        writer
            .append_message("s-new", "user", "hello")
            .await
            .expect("append message");

        assert!(reader.session_exists("s-new").await);
        let lines = read_lines(&tmp.path().join("sessions/s-new.jsonl")).await;
        assert_eq!(lines.len(), 1);
        let value: serde_json::Value = serde_json::from_str(&lines[0]).expect("json");
        assert_eq!(value["type"], "message");
        assert_eq!(value["message"]["content"], "hello");
    }

    #[tokio::test]
    async fn test_roundtrip_all_entry_types() {
        let tmp = TempDir::new().expect("tempdir");
        let writer = SessionWriter::new(tmp.path());
        let reader = SessionReader::new(tmp.path());

        writer
            .append(
                "s1",
                SessionEntry::Session {
                    version: 1,
                    id: "s1".to_owned(),
                    timestamp: Utc::now(),
                    agent_id: "main".to_owned(),
                },
            )
            .await
            .expect("session");
        writer
            .append(
                "s1",
                SessionEntry::Message {
                    id: "m1".to_owned(),
                    timestamp: Utc::now(),
                    message: SessionMessage {
                        role: "user".to_owned(),
                        content: "hello".to_owned(),
                    },
                },
            )
            .await
            .expect("message");
        writer
            .append(
                "s1",
                SessionEntry::ToolCall {
                    id: "tc1".to_owned(),
                    timestamp: Utc::now(),
                    tool: "search".to_owned(),
                    input: serde_json::json!({"q": "rust"}),
                },
            )
            .await
            .expect("toolcall");
        writer
            .append(
                "s1",
                SessionEntry::ToolResult {
                    id: "tr1".to_owned(),
                    timestamp: Utc::now(),
                    tool: "search".to_owned(),
                    output: serde_json::json!({"ok": true}),
                },
            )
            .await
            .expect("toolresult");
        writer
            .append(
                "s1",
                SessionEntry::Compaction {
                    id: "c1".to_owned(),
                    timestamp: Utc::now(),
                    summary: "sum".to_owned(),
                    dropped_before: "m1".to_owned(),
                },
            )
            .await
            .expect("compaction");
        writer
            .append(
                "s1",
                SessionEntry::ModelChange {
                    id: "mc1".to_owned(),
                    timestamp: Utc::now(),
                    model: "gpt-x".to_owned(),
                },
            )
            .await
            .expect("model change");

        let entries = reader.load_all_entries("s1").await.expect("load entries");
        assert_eq!(entries.len(), 6);
        assert!(matches!(entries[0], SessionEntry::Session { .. }));
        assert!(matches!(entries[1], SessionEntry::Message { .. }));
        assert!(matches!(entries[2], SessionEntry::ToolCall { .. }));
        assert!(matches!(entries[3], SessionEntry::ToolResult { .. }));
        assert!(matches!(entries[4], SessionEntry::Compaction { .. }));
        assert!(matches!(entries[5], SessionEntry::ModelChange { .. }));

        let lines = read_lines(&tmp.path().join("sessions/s1.jsonl")).await;
        for line in lines {
            let value: serde_json::Value = serde_json::from_str(&line).expect("valid jsonl");
            assert!(value.get("type").is_some());
        }
    }

    #[tokio::test]
    async fn test_clear_session() {
        let tmp = TempDir::new().expect("tempdir");
        let writer = SessionWriter::new(tmp.path());
        let reader = SessionReader::new(tmp.path());

        // Clear non-existent session returns false
        let cleared = writer.clear_session("nonexistent").await.expect("clear");
        assert!(!cleared);

        // Create a session
        writer.start_session("s1", "main").await.expect("start");
        writer
            .append_message("s1", "user", "hello")
            .await
            .expect("append");
        assert!(reader.session_exists("s1").await);

        // Clear existing session returns true
        let cleared = writer.clear_session("s1").await.expect("clear");
        assert!(cleared);
        assert!(!reader.session_exists("s1").await);

        // Clear again returns false
        let cleared = writer.clear_session("s1").await.expect("clear again");
        assert!(!cleared);
    }
}
