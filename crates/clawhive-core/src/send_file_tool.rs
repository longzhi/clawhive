use std::path::Path;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use clawhive_provider::ToolDef;
use clawhive_schema::{Attachment, AttachmentKind};
use serde_json::json;

use super::tool::{ToolContext, ToolExecutor, ToolOutput};

pub struct SendFileTool;

impl SendFileTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for SendFileTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ToolExecutor for SendFileTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "send_file".to_string(),
            description: "Send a local file (image, document, etc.) to the current chat. \
                The file will be uploaded and delivered as an attachment in the conversation."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Absolute path to the local file"
                    },
                    "name": {
                        "type": "string",
                        "description": "Display file name (defaults to the file's basename)"
                    },
                    "mime_type": {
                        "type": "string",
                        "description": "MIME type (auto-detected from extension if omitted)"
                    }
                },
                "required": ["path"]
            }),
        }
    }

    async fn execute(&self, input: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let path_str = input
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("missing required parameter: path"))?;

        let path = Path::new(path_str);
        if !path.is_absolute() {
            return Err(anyhow!("path must be absolute: {path_str}"));
        }

        let metadata = tokio::fs::metadata(path)
            .await
            .map_err(|e| anyhow!("cannot access file {path_str}: {e}"))?;

        if !metadata.is_file() {
            return Err(anyhow!("{path_str} is not a regular file"));
        }

        let file_size = metadata.len();
        const MAX_FILE_SIZE: u64 = 50 * 1024 * 1024;
        if file_size > MAX_FILE_SIZE {
            return Err(anyhow!(
                "file too large: {} bytes (max {} bytes)",
                file_size,
                MAX_FILE_SIZE
            ));
        }

        let file_name = input
            .get("name")
            .and_then(|v| v.as_str())
            .map(String::from)
            .or_else(|| path.file_name().map(|n| n.to_string_lossy().into_owned()));

        let mime_type = input
            .get("mime_type")
            .and_then(|v| v.as_str())
            .map(String::from)
            .or_else(|| guess_mime_from_extension(path));

        let kind = mime_type
            .as_deref()
            .map(kind_from_mime)
            .unwrap_or(AttachmentKind::Document);

        let attachment = Attachment {
            kind,
            url: path_str.to_string(),
            mime_type,
            file_name: file_name.clone(),
            size: Some(file_size),
        };

        let collector = ctx
            .attachment_collector()
            .ok_or_else(|| anyhow!("send_file is not available in this context"))?;

        collector.lock().await.push(attachment);

        let display = file_name.as_deref().unwrap_or(path_str);
        Ok(ToolOutput {
            content: format!("Queued file for sending: {display} ({file_size} bytes)"),
            is_error: false,
        })
    }
}

fn guess_mime_from_extension(path: &Path) -> Option<String> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    let mime = match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "pdf" => "application/pdf",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "csv" => "text/csv",
        "txt" => "text/plain",
        "json" => "application/json",
        "zip" => "application/zip",
        "mp4" => "video/mp4",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" => "audio/ogg",
        _ => return None,
    };
    Some(mime.to_string())
}

fn kind_from_mime(mime: &str) -> AttachmentKind {
    if mime.starts_with("image/") {
        AttachmentKind::Image
    } else if mime.starts_with("video/") {
        AttachmentKind::Video
    } else if mime.starts_with("audio/") {
        AttachmentKind::Audio
    } else {
        AttachmentKind::Document
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn definition_has_correct_name() {
        let tool = SendFileTool::new();
        let def = tool.definition();
        assert_eq!(def.name, "send_file");
    }

    #[tokio::test]
    async fn rejects_missing_path() {
        let tool = SendFileTool::new();
        let ctx = ToolContext::builtin();
        let result = tool.execute(json!({}), &ctx).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn rejects_relative_path() {
        let tool = SendFileTool::new();
        let ctx = ToolContext::builtin();
        let result = tool
            .execute(json!({"path": "relative/file.txt"}), &ctx)
            .await;
        match result {
            Err(e) => assert!(e.to_string().contains("absolute")),
            Ok(_) => panic!("should reject relative path"),
        }
    }

    #[tokio::test]
    async fn rejects_nonexistent_file() {
        let tool = SendFileTool::new();
        let ctx = ToolContext::builtin();
        let result = tool
            .execute(json!({"path": "/nonexistent/file.txt"}), &ctx)
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn rejects_when_no_collector() {
        let tool = SendFileTool::new();
        let ctx = ToolContext::builtin();

        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("test.txt");
        tokio::fs::write(&file_path, "hello").await.unwrap();

        let result = tool
            .execute(json!({"path": file_path.to_str().unwrap()}), &ctx)
            .await;
        match result {
            Err(e) => assert!(e.to_string().contains("not available")),
            Ok(_) => panic!("should reject when no collector"),
        }
    }

    #[tokio::test]
    async fn queues_file_with_collector() {
        let tool = SendFileTool::new();
        let collector = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let ctx = ToolContext::builtin().with_attachment_collector(collector.clone());

        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("photo.png");
        tokio::fs::write(&file_path, b"fake png data")
            .await
            .unwrap();

        let result = tool
            .execute(json!({"path": file_path.to_str().unwrap()}), &ctx)
            .await
            .unwrap();

        assert!(!result.is_error);
        assert!(result.content.contains("Queued file"));

        let attachments = collector.lock().await;
        assert_eq!(attachments.len(), 1);
        assert_eq!(attachments[0].kind, AttachmentKind::Image);
        assert_eq!(attachments[0].mime_type, Some("image/png".to_string()));
    }

    #[test]
    fn guess_mime_common_types() {
        assert_eq!(
            guess_mime_from_extension(Path::new("photo.jpg")),
            Some("image/jpeg".to_string())
        );
        assert_eq!(
            guess_mime_from_extension(Path::new("doc.pdf")),
            Some("application/pdf".to_string())
        );
        assert_eq!(
            guess_mime_from_extension(Path::new("video.mp4")),
            Some("video/mp4".to_string())
        );
        assert_eq!(guess_mime_from_extension(Path::new("file.xyz")), None);
    }

    #[test]
    fn kind_from_mime_categorizes_correctly() {
        assert_eq!(kind_from_mime("image/png"), AttachmentKind::Image);
        assert_eq!(kind_from_mime("video/mp4"), AttachmentKind::Video);
        assert_eq!(kind_from_mime("audio/mpeg"), AttachmentKind::Audio);
        assert_eq!(kind_from_mime("application/pdf"), AttachmentKind::Document);
    }
}
