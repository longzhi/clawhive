use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use clawhive_provider::ToolDef;

use super::access_gate::{resolve_path, AccessGate, AccessLevel, AccessResult};
use super::tool::{ToolContext, ToolExecutor, ToolOutput};

/// Check the AccessGate and return an error ToolOutput if access is not allowed.
/// Returns `None` when the access is allowed.
async fn gate_check(
    gate: &AccessGate,
    resolved: &Path,
    level: AccessLevel,
    _path_str: &str,
) -> Option<ToolOutput> {
    match gate.check(resolved, level).await {
        AccessResult::Allowed => None,
        AccessResult::Denied(reason) => Some(ToolOutput {
            content: reason,
            is_error: true,
        }),
        AccessResult::NeedGrant { dir, need } => {
            let dir_path = Path::new(&dir);
            match gate.try_auto_grant(dir_path, need).await {
                Ok(()) => {
                    tracing::info!(dir = %dir, level = %need, "auto-granted access to directory");
                    match gate.check(resolved, level).await {
                        AccessResult::Allowed => None,
                        other => Some(ToolOutput {
                            content: format!(
                                "Auto-grant succeeded but access is still not allowed: {other:?}"
                            ),
                            is_error: true,
                        }),
                    }
                }
                Err(e) => {
                    tracing::debug!(dir = %dir, error = %e, "auto-grant failed, requiring manual grant_access");
                    Some(ToolOutput {
                        content: format!(
                            "Access denied: directory {dir} is not authorized for {need} access. Auto-grant was blocked ({e}). Call the grant_access tool with path=\"{dir}\" and level=\"{need}\" to request access, then retry."
                        ),
                        is_error: true,
                    })
                }
            }
        }
    }
}

// ───────────────────────────── ReadFileTool ───────────────────────

pub struct ReadFileTool {
    workspace: PathBuf,
    gate: Arc<AccessGate>,
}

impl ReadFileTool {
    pub fn new(workspace: PathBuf, gate: Arc<AccessGate>) -> Self {
        Self { workspace, gate }
    }
}

#[async_trait]
impl ToolExecutor for ReadFileTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "read_file".into(),
            description: "Read a file. Returns its content. Supports offset and limit for large files. Works with workspace-relative or absolute paths.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "File path relative to workspace root, or an absolute path"
                    },
                    "offset": {
                        "type": "integer",
                        "description": "Line number to start from (1-indexed, default: 1)"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of lines to return (default: 200)"
                    }
                },
                "required": ["path"]
            }),
        }
    }

    async fn execute(&self, input: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let path_str = input["path"]
            .as_str()
            .ok_or_else(|| anyhow!("missing 'path' field"))?;
        let offset = input["offset"].as_u64().unwrap_or(1).max(1) as usize;
        let limit = input["limit"].as_u64().unwrap_or(200) as usize;

        let resolved = match resolve_path(&self.workspace, path_str) {
            Ok(p) => p,
            Err(e) => {
                return Ok(ToolOutput {
                    content: format!("Error: {e}"),
                    is_error: true,
                })
            }
        };

        // AccessGate check (includes HardBaseline)
        if let Some(denied) = gate_check(&self.gate, &resolved, AccessLevel::Ro, path_str).await {
            return Ok(denied);
        }

        // External skill policy check (ToolContext)
        let resolved_str = resolved.to_str().unwrap_or("");
        let requested_abs = if Path::new(path_str).is_absolute() {
            path_str.to_string()
        } else {
            self.workspace.join(path_str).to_string_lossy().into_owned()
        };
        if !ctx.check_read(resolved_str) && !ctx.check_read(&requested_abs) {
            return Ok(ToolOutput {
                content: format!("Read access denied for path: {}", path_str),
                is_error: true,
            });
        }

        if resolved.is_dir() {
            match std::fs::read_dir(&resolved) {
                Ok(entries) => {
                    let mut listing = Vec::new();
                    for entry in entries.flatten() {
                        let name = entry.file_name().to_string_lossy().to_string();
                        let suffix = if entry.path().is_dir() { "/" } else { "" };
                        listing.push(format!("{name}{suffix}"));
                    }
                    listing.sort();
                    Ok(ToolOutput {
                        content: listing.join("\n"),
                        is_error: false,
                    })
                }
                Err(e) => Ok(ToolOutput {
                    content: format!("Error reading directory: {e}"),
                    is_error: true,
                }),
            }
        } else {
            match tokio::fs::read_to_string(&resolved).await {
                Ok(content) => {
                    let lines: Vec<&str> = content.lines().collect();
                    let start = (offset - 1).min(lines.len());
                    let end = (start + limit).min(lines.len());
                    let total = lines.len();

                    let mut output = String::new();
                    for (i, line) in lines[start..end].iter().enumerate() {
                        output.push_str(&format!("{}: {}\n", start + i + 1, line));
                    }
                    if end < total {
                        output.push_str(&format!(
                            "\n(showing lines {}-{} of {}. Use offset={} to read more)\n",
                            start + 1,
                            end,
                            total,
                            end + 1
                        ));
                    }
                    Ok(ToolOutput {
                        content: output,
                        is_error: false,
                    })
                }
                Err(e) => Ok(ToolOutput {
                    content: format!("Error reading file: {e}"),
                    is_error: true,
                }),
            }
        }
    }
}

// ───────────────────────────── WriteFileTool ──────────────────────

pub struct WriteFileTool {
    workspace: PathBuf,
    gate: Arc<AccessGate>,
}

impl WriteFileTool {
    pub fn new(workspace: PathBuf, gate: Arc<AccessGate>) -> Self {
        Self { workspace, gate }
    }
}

#[async_trait]
impl ToolExecutor for WriteFileTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "write_file".into(),
            description: "Write content to a file. Creates the file and parent directories if they don't exist. Overwrites existing content. Works with workspace-relative or absolute paths.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "File path relative to workspace root, or an absolute path"
                    },
                    "content": {
                        "type": "string",
                        "description": "The content to write"
                    }
                },
                "required": ["path", "content"]
            }),
        }
    }

    async fn execute(&self, input: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let path_str = input["path"]
            .as_str()
            .ok_or_else(|| anyhow!("missing 'path' field"))?;
        let content = input["content"]
            .as_str()
            .ok_or_else(|| anyhow!("missing 'content' field"))?;

        let resolved = match resolve_path(&self.workspace, path_str) {
            Ok(p) => p,
            Err(e) => {
                return Ok(ToolOutput {
                    content: format!("Error: {e}"),
                    is_error: true,
                })
            }
        };

        // AccessGate check (includes HardBaseline)
        if let Some(denied) = gate_check(&self.gate, &resolved, AccessLevel::Rw, path_str).await {
            return Ok(denied);
        }

        // External skill policy check
        let resolved_str = resolved.to_str().unwrap_or("");
        let requested_abs = if Path::new(path_str).is_absolute() {
            path_str.to_string()
        } else {
            self.workspace.join(path_str).to_string_lossy().into_owned()
        };
        if !ctx.check_write(resolved_str) && !ctx.check_write(&requested_abs) {
            return Ok(ToolOutput {
                content: format!("Write access denied for path: {}", path_str),
                is_error: true,
            });
        }

        if let Some(parent) = resolved.parent() {
            if let Err(e) = tokio::fs::create_dir_all(parent).await {
                return Ok(ToolOutput {
                    content: format!("Error creating directories: {e}"),
                    is_error: true,
                });
            }
        }

        match tokio::fs::write(&resolved, content).await {
            Ok(()) => Ok(ToolOutput {
                content: format!("Written {} bytes to {}", content.len(), path_str),
                is_error: false,
            }),
            Err(e) => Ok(ToolOutput {
                content: format!("Error writing file: {e}"),
                is_error: true,
            }),
        }
    }
}

// ───────────────────────────── EditFileTool ──────────────────────

pub struct EditFileTool {
    workspace: PathBuf,
    gate: Arc<AccessGate>,
}

impl EditFileTool {
    pub fn new(workspace: PathBuf, gate: Arc<AccessGate>) -> Self {
        Self { workspace, gate }
    }
}

#[async_trait]
impl ToolExecutor for EditFileTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "edit_file".into(),
            description: "Edit a file by replacing an exact string match. The old_text must appear exactly once in the file.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "File path relative to workspace root, or an absolute path"
                    },
                    "old_text": {
                        "type": "string",
                        "description": "The exact text to find and replace"
                    },
                    "new_text": {
                        "type": "string",
                        "description": "The replacement text"
                    }
                },
                "required": ["path", "old_text", "new_text"]
            }),
        }
    }

    async fn execute(&self, input: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let path_str = input["path"]
            .as_str()
            .ok_or_else(|| anyhow!("missing 'path' field"))?;
        let old_text = input["old_text"]
            .as_str()
            .ok_or_else(|| anyhow!("missing 'old_text' field"))?;
        let new_text = input["new_text"]
            .as_str()
            .ok_or_else(|| anyhow!("missing 'new_text' field"))?;

        let resolved = match resolve_path(&self.workspace, path_str) {
            Ok(p) => p,
            Err(e) => {
                return Ok(ToolOutput {
                    content: format!("Error: {e}"),
                    is_error: true,
                })
            }
        };

        // AccessGate check (includes HardBaseline)
        if let Some(denied) = gate_check(&self.gate, &resolved, AccessLevel::Rw, path_str).await {
            return Ok(denied);
        }

        // External skill policy check
        let resolved_str = resolved.to_str().unwrap_or("");
        let requested_abs = if Path::new(path_str).is_absolute() {
            path_str.to_string()
        } else {
            self.workspace.join(path_str).to_string_lossy().into_owned()
        };
        if !ctx.check_write(resolved_str) && !ctx.check_write(&requested_abs) {
            return Ok(ToolOutput {
                content: format!("Write access denied for path: {}", path_str),
                is_error: true,
            });
        }

        let content = match tokio::fs::read_to_string(&resolved).await {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolOutput {
                    content: format!("Error reading file: {e}"),
                    is_error: true,
                })
            }
        };

        let count = content.matches(old_text).count();
        if count == 0 {
            return Ok(ToolOutput {
                content: "old_text not found in file".into(),
                is_error: true,
            });
        }
        if count > 1 {
            return Ok(ToolOutput {
                content: format!(
                    "Found {count} matches for old_text. Provide more context to make it unique."
                ),
                is_error: true,
            });
        }

        let new_content = content.replacen(old_text, new_text, 1);
        match tokio::fs::write(&resolved, &new_content).await {
            Ok(()) => Ok(ToolOutput {
                content: format!("Edit applied to {path_str}"),
                is_error: false,
            }),
            Err(e) => Ok(ToolOutput {
                content: format!("Error writing file: {e}"),
                is_error: true,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_gate(workspace: &Path) -> Arc<AccessGate> {
        Arc::new(AccessGate::in_memory(workspace.to_path_buf()))
    }

    #[tokio::test]
    async fn read_file_basic() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("hello.txt"), "line1\nline2\nline3").unwrap();
        let gate = make_gate(tmp.path());
        let tool = ReadFileTool::new(tmp.path().to_path_buf(), gate);
        let ctx = ToolContext::builtin();
        let result = tool
            .execute(serde_json::json!({"path": "hello.txt"}), &ctx)
            .await
            .unwrap();
        assert!(!result.is_error);
        assert!(result.content.contains("line1"));
        assert!(result.content.contains("line3"));
    }

    #[tokio::test]
    async fn read_file_offset() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("f.txt"), "a\nb\nc\nd\ne").unwrap();
        let gate = make_gate(tmp.path());
        let tool = ReadFileTool::new(tmp.path().to_path_buf(), gate);
        let ctx = ToolContext::builtin();
        let result = tool
            .execute(
                serde_json::json!({"path": "f.txt", "offset": 3, "limit": 2}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!result.is_error);
        assert!(result.content.contains("3: c"));
        assert!(result.content.contains("4: d"));
        assert!(!result.content.contains("1: a"));
    }

    #[tokio::test]
    async fn read_dir_listing() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "").unwrap();
        std::fs::create_dir(tmp.path().join("subdir")).unwrap();
        let gate = make_gate(tmp.path());
        let tool = ReadFileTool::new(tmp.path().to_path_buf(), gate);
        let ctx = ToolContext::builtin();
        let result = tool
            .execute(serde_json::json!({"path": "."}), &ctx)
            .await
            .unwrap();
        assert!(!result.is_error);
        assert!(result.content.contains("a.txt"));
        assert!(result.content.contains("subdir/"));
    }

    #[tokio::test]
    async fn read_file_denied_by_policy() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("secret.txt"), "classified").unwrap();

        let gate = make_gate(tmp.path());
        let tool = ReadFileTool::new(tmp.path().to_path_buf(), gate);

        let perms = corral_core::Permissions::builder()
            .fs_read([format!("{}/**/*.md", tmp.path().display())])
            .build();
        let ctx = ToolContext::external(perms);

        let result = tool
            .execute(serde_json::json!({"path": "secret.txt"}), &ctx)
            .await
            .unwrap();
        assert!(result.is_error);
        assert!(result.content.contains("denied"));
    }

    #[tokio::test]
    async fn read_file_allowed_by_policy() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("readme.md"), "hello").unwrap();

        let gate = make_gate(tmp.path());
        let tool = ReadFileTool::new(tmp.path().to_path_buf(), gate);

        let perms = corral_core::Permissions::builder()
            .fs_read([format!("{}/**", tmp.path().display())])
            .build();
        let ctx = ToolContext::external(perms);

        let result = tool
            .execute(serde_json::json!({"path": "readme.md"}), &ctx)
            .await
            .unwrap();
        assert!(!result.is_error);
        assert!(result.content.contains("hello"));
    }

    #[tokio::test]
    async fn outside_workspace_auto_granted_on_first_access() {
        let tmp = TempDir::new().unwrap();
        // Create an "outside" directory
        let outside = tmp.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("file.txt"), "external").unwrap();

        // Workspace is a sibling directory
        let ws = tmp.path().join("workspace");
        std::fs::create_dir(&ws).unwrap();
        let gate = make_gate(&ws);
        let target = outside.join("file.txt");

        let before = gate.check(&target, AccessLevel::Ro).await;
        assert!(matches!(before, AccessResult::NeedGrant { .. }));

        let tool = ReadFileTool::new(ws, gate.clone());
        let ctx = ToolContext::builtin();
        let result = tool
            .execute(serde_json::json!({"path": target.to_str().unwrap()}), &ctx)
            .await
            .unwrap();
        assert!(!result.is_error);
        assert!(result.content.contains("external"));

        let after = gate.check(&target, AccessLevel::Ro).await;
        assert_eq!(after, AccessResult::Allowed);
    }

    #[tokio::test]
    async fn outside_workspace_subsequent_access_uses_existing_auto_grant() {
        let tmp = TempDir::new().unwrap();
        let outside = tmp.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        let file = outside.join("file.txt");
        std::fs::write(&file, "external data").unwrap();

        let ws = tmp.path().join("workspace");
        std::fs::create_dir(&ws).unwrap();
        let gate = make_gate(&ws);
        let tool = ReadFileTool::new(ws, gate.clone());
        let ctx = ToolContext::builtin();

        let first = tool
            .execute(serde_json::json!({"path": file.to_str().unwrap()}), &ctx)
            .await
            .unwrap();
        assert!(!first.is_error);

        let second = tool
            .execute(serde_json::json!({"path": file.to_str().unwrap()}), &ctx)
            .await
            .unwrap();
        assert!(!second.is_error);
        assert!(second.content.contains("external data"));

        let entries = gate.list().await;
        assert_eq!(entries.len(), 1);
    }

    #[tokio::test]
    async fn sensitive_path_still_returns_error() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().join("workspace");
        std::fs::create_dir(&ws).unwrap();
        let gate = make_gate(&ws);
        let tool = ReadFileTool::new(ws, gate);
        let ctx = ToolContext::builtin();

        let result = tool
            .execute(serde_json::json!({"path": "/home/user/.ssh/id_rsa"}), &ctx)
            .await
            .unwrap();
        assert!(result.is_error);
        assert!(result.content.contains("denied"));
    }

    #[tokio::test]
    async fn outside_workspace_allowed_after_grant() {
        let tmp = TempDir::new().unwrap();
        let outside = tmp.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("file.txt"), "external data").unwrap();

        let ws = tmp.path().join("workspace");
        std::fs::create_dir(&ws).unwrap();
        let gate = make_gate(&ws);
        gate.grant(&outside, AccessLevel::Ro).await.unwrap();
        let tool = ReadFileTool::new(ws, gate);
        let ctx = ToolContext::builtin();
        let result = tool
            .execute(
                serde_json::json!({"path": outside.join("file.txt").to_str().unwrap()}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!result.is_error);
        assert!(result.content.contains("external data"));
    }

    #[tokio::test]
    async fn write_file_basic() {
        let tmp = TempDir::new().unwrap();
        let gate = make_gate(tmp.path());
        let tool = WriteFileTool::new(tmp.path().to_path_buf(), gate);
        let ctx = ToolContext::builtin();
        let result = tool
            .execute(
                serde_json::json!({"path": "new.txt", "content": "hello world"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!result.is_error, "unexpected error: {}", result.content);
        let content = std::fs::read_to_string(tmp.path().join("new.txt")).unwrap();
        assert_eq!(content, "hello world");
    }

    #[tokio::test]
    async fn write_file_creates_dirs() {
        let tmp = TempDir::new().unwrap();
        let gate = make_gate(tmp.path());
        let tool = WriteFileTool::new(tmp.path().to_path_buf(), gate);
        let ctx = ToolContext::builtin();
        let result = tool
            .execute(
                serde_json::json!({"path": "sub/deep/file.txt", "content": "nested"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!result.is_error);
        assert!(tmp.path().join("sub/deep/file.txt").exists());
    }

    #[tokio::test]
    async fn write_file_denied_by_policy() {
        let tmp = TempDir::new().unwrap();
        let gate = make_gate(tmp.path());
        let tool = WriteFileTool::new(tmp.path().to_path_buf(), gate);

        let perms = corral_core::Permissions::builder()
            .fs_write([format!("{}/**/*.log", tmp.path().display())])
            .build();
        let ctx = ToolContext::external(perms);

        let result = tool
            .execute(
                serde_json::json!({"path": "hack.sh", "content": "rm -rf /"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(result.is_error);
        assert!(result.content.contains("denied"));
    }

    #[tokio::test]
    async fn edit_file_basic() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("e.txt"), "foo bar baz").unwrap();
        let gate = make_gate(tmp.path());
        let tool = EditFileTool::new(tmp.path().to_path_buf(), gate);
        let ctx = ToolContext::builtin();
        let result = tool
            .execute(
                serde_json::json!({"path": "e.txt", "old_text": "bar", "new_text": "qux"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!result.is_error);
        let content = std::fs::read_to_string(tmp.path().join("e.txt")).unwrap();
        assert_eq!(content, "foo qux baz");
    }

    #[tokio::test]
    async fn edit_file_not_found() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("e.txt"), "foo bar").unwrap();
        let gate = make_gate(tmp.path());
        let tool = EditFileTool::new(tmp.path().to_path_buf(), gate);
        let ctx = ToolContext::builtin();
        let result = tool
            .execute(
                serde_json::json!({"path": "e.txt", "old_text": "missing", "new_text": "x"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(result.is_error);
        assert!(result.content.contains("not found"));
    }

    #[tokio::test]
    async fn edit_file_multiple_matches() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("e.txt"), "aaa aaa").unwrap();
        let gate = make_gate(tmp.path());
        let tool = EditFileTool::new(tmp.path().to_path_buf(), gate);
        let ctx = ToolContext::builtin();
        let result = tool
            .execute(
                serde_json::json!({"path": "e.txt", "old_text": "aaa", "new_text": "bbb"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(result.is_error);
        assert!(result.content.contains("2 matches"));
    }

    #[tokio::test]
    async fn edit_file_denied_by_policy() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("config.yaml"), "key: value").unwrap();
        let gate = make_gate(tmp.path());
        let tool = EditFileTool::new(tmp.path().to_path_buf(), gate);

        let perms = corral_core::Permissions::builder()
            .fs_read([format!("{}/**", tmp.path().display())])
            .build();
        let ctx = ToolContext::external(perms);

        let result = tool
            .execute(
                serde_json::json!({"path": "config.yaml", "old_text": "key", "new_text": "newkey"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(result.is_error);
        assert!(result.content.contains("denied"));
    }
}
