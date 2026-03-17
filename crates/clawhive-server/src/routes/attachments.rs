//! Attachment upload / download endpoints for the web console.
//!
//! Files are stored on disk at `{root}/data/attachments/{uuid}` with metadata
//! sidecar files at `{root}/data/attachments/{uuid}.json`.

use std::path::{Path, PathBuf};

use axum::{
    extract::{Multipart, Path as AxumPath, State},
    http::{header, HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use base64::Engine;
use chrono::Utc;
use clawhive_schema::{Attachment, AttachmentKind};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::state::AppState;
use crate::{extract_session_token, is_valid_session};

// ── Constants ───────────────────────────────────────────────────────────────

const MAX_FILE_SIZE: usize = 20 * 1024 * 1024; // 20 MB

const DENIED_EXTENSIONS: &[&str] = &[
    "exe", "bat", "cmd", "sh", "ps1", "msi", "dll", "so", "dylib", "com", "vbs", "vbe", "wsf",
    "wsh", "scr", "pif",
];

// ── Types ───────────────────────────────────────────────────────────────────

/// Metadata persisted alongside each uploaded file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachmentMeta {
    pub id: String,
    #[serde(default)]
    pub conversation_id: Option<String>,
    pub kind: String,
    pub mime_type: String,
    pub file_name: String,
    pub size: u64,
    pub created_at: String,
}

// ── Router ──────────────────────────────────────────────────────────────────

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", post(upload_attachment))
        .route("/{id}", get(download_attachment).delete(delete_attachment))
}

// ── Handlers ────────────────────────────────────────────────────────────────

async fn upload_attachment(
    State(state): State<AppState>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> Result<Json<AttachmentMeta>, (StatusCode, Json<serde_json::Value>)> {
    require_auth(&state, &headers)?;

    let mut file_data: Option<(String, String, Vec<u8>)> = None;
    let mut conversation_id: Option<String> = None;

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| bad_request(format!("Invalid multipart data: {e}")))?
    {
        let field_name = field.name().unwrap_or("").to_string();
        match field_name.as_str() {
            "file" => {
                let file_name = field.file_name().unwrap_or("unnamed").to_string();
                let mime_type = field
                    .content_type()
                    .unwrap_or("application/octet-stream")
                    .to_string();
                let data = field
                    .bytes()
                    .await
                    .map_err(|e| bad_request(format!("Failed to read file data: {e}")))?;
                file_data = Some((file_name, mime_type, data.to_vec()));
            }
            "conversation_id" => {
                let text = field
                    .text()
                    .await
                    .map_err(|e| bad_request(format!("Failed to read conversation_id: {e}")))?;
                if !text.is_empty() {
                    conversation_id = Some(text);
                }
            }
            _ => {}
        }
    }

    let (file_name, mime_type, data) =
        file_data.ok_or_else(|| bad_request("No file field found".to_string()))?;

    if is_denied_extension(&file_name) {
        return Err(bad_request("File type not allowed".to_string()));
    }
    if data.len() > MAX_FILE_SIZE {
        return Err(bad_request(format!(
            "File exceeds {} MB limit",
            MAX_FILE_SIZE / (1024 * 1024)
        )));
    }

    let id = Uuid::new_v4().to_string();
    let dir = attachments_dir(&state.root);
    std::fs::create_dir_all(&dir)
        .map_err(|e| server_error(format!("Failed to create storage directory: {e}")))?;

    std::fs::write(attachment_data_path(&state.root, &id), &data)
        .map_err(|e| server_error(format!("Failed to save file: {e}")))?;

    let meta = AttachmentMeta {
        id: id.clone(),
        conversation_id,
        kind: kind_from_mime(&mime_type),
        mime_type,
        file_name,
        size: data.len() as u64,
        created_at: Utc::now().to_rfc3339(),
    };

    let meta_json = serde_json::to_string_pretty(&meta)
        .map_err(|e| server_error(format!("Failed to serialize metadata: {e}")))?;

    if let Err(e) = std::fs::write(attachment_meta_path(&state.root, &id), meta_json) {
        let _ = std::fs::remove_file(attachment_data_path(&state.root, &id));
        return Err(server_error(format!("Failed to save metadata: {e}")));
    }

    tracing::info!(
        attachment_id = %meta.id,
        file_name = %meta.file_name,
        mime_type = %meta.mime_type,
        size = meta.size,
        "attachment uploaded"
    );

    Ok(Json(meta))
}

async fn download_attachment(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Result<impl IntoResponse, StatusCode> {
    require_auth(&state, &headers).map_err(|r| r.0)?;

    let meta = read_meta(&state.root, &id).ok_or(StatusCode::NOT_FOUND)?;
    let data =
        std::fs::read(attachment_data_path(&state.root, &id)).map_err(|_| StatusCode::NOT_FOUND)?;

    Ok((
        [
            (header::CONTENT_TYPE, meta.mime_type),
            (
                header::CONTENT_DISPOSITION,
                format!("inline; filename=\"{}\"", meta.file_name),
            ),
        ],
        data,
    ))
}

async fn delete_attachment(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Result<StatusCode, StatusCode> {
    require_auth(&state, &headers).map_err(|r| r.0)?;
    remove_attachment_files(&state.root, &id);
    Ok(StatusCode::NO_CONTENT)
}

// ── Public helpers (used by chat.rs) ────────────────────────────────────────

/// Resolve an uploaded attachment to the schema `Attachment` type by reading the
/// file from disk and base64-encoding its contents.
pub fn resolve_to_attachment(root: &Path, id: &str) -> Option<Attachment> {
    let meta = read_meta(root, id)?;
    let data = std::fs::read(attachment_data_path(root, id)).ok()?;
    let base64_data = base64::engine::general_purpose::STANDARD.encode(&data);

    Some(Attachment {
        kind: parse_attachment_kind(&meta.kind),
        url: base64_data,
        mime_type: Some(meta.mime_type),
        file_name: Some(meta.file_name),
        size: Some(meta.size),
    })
}

/// Delete all attachment files associated with a conversation.
pub fn delete_conversation_attachments(root: &Path, conversation_id: &str) {
    let dir = attachments_dir(root);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };

    let mut deleted = 0usize;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(meta) = serde_json::from_str::<AttachmentMeta>(&content) else {
            continue;
        };
        if meta.conversation_id.as_deref() == Some(conversation_id) {
            remove_attachment_files(root, &meta.id);
            deleted += 1;
        }
    }

    if deleted > 0 {
        tracing::info!(
            conversation_id = %conversation_id,
            deleted_count = deleted,
            "cleaned up conversation attachments"
        );
    }
}

// ── Internal helpers ────────────────────────────────────────────────────────

fn attachments_dir(root: &Path) -> PathBuf {
    root.join("data/attachments")
}

fn attachment_data_path(root: &Path, id: &str) -> PathBuf {
    attachments_dir(root).join(id)
}

fn attachment_meta_path(root: &Path, id: &str) -> PathBuf {
    attachments_dir(root).join(format!("{id}.json"))
}

fn read_meta(root: &Path, id: &str) -> Option<AttachmentMeta> {
    let content = std::fs::read_to_string(attachment_meta_path(root, id)).ok()?;
    serde_json::from_str(&content).ok()
}

fn remove_attachment_files(root: &Path, id: &str) {
    let _ = std::fs::remove_file(attachment_data_path(root, id));
    let _ = std::fs::remove_file(attachment_meta_path(root, id));
}

fn kind_from_mime(mime: &str) -> String {
    if mime.starts_with("image/") {
        "image".to_string()
    } else if mime.starts_with("video/") {
        "video".to_string()
    } else if mime.starts_with("audio/") {
        "audio".to_string()
    } else {
        "document".to_string()
    }
}

fn parse_attachment_kind(kind: &str) -> AttachmentKind {
    match kind {
        "image" => AttachmentKind::Image,
        "video" => AttachmentKind::Video,
        "audio" => AttachmentKind::Audio,
        "document" => AttachmentKind::Document,
        _ => AttachmentKind::Other,
    }
}

fn is_denied_extension(file_name: &str) -> bool {
    let ext = file_name.rsplit('.').next().unwrap_or("").to_lowercase();
    DENIED_EXTENSIONS.contains(&ext.as_str())
}

fn require_auth(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    let token = extract_session_token(headers).ok_or((
        StatusCode::UNAUTHORIZED,
        Json(serde_json::json!({"error": "Authentication required"})),
    ))?;
    if !is_valid_session(state, &token) {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "Authentication required"})),
        ));
    }
    Ok(())
}

fn bad_request(message: String) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({"error": message})),
    )
}

fn server_error(message: String) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({"error": message})),
    )
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_from_mime_classifies_correctly() {
        assert_eq!(kind_from_mime("image/png"), "image");
        assert_eq!(kind_from_mime("image/jpeg"), "image");
        assert_eq!(kind_from_mime("video/mp4"), "video");
        assert_eq!(kind_from_mime("audio/mpeg"), "audio");
        assert_eq!(kind_from_mime("application/pdf"), "document");
        assert_eq!(kind_from_mime("text/plain"), "document");
    }

    #[test]
    fn is_denied_extension_blocks_executables() {
        assert!(is_denied_extension("virus.exe"));
        assert!(is_denied_extension("script.bat"));
        assert!(is_denied_extension("HACK.PS1"));
        assert!(is_denied_extension("lib.dll"));
        assert!(!is_denied_extension("report.pdf"));
        assert!(!is_denied_extension("photo.png"));
        assert!(!is_denied_extension("data.csv"));
        assert!(!is_denied_extension("noextension"));
    }

    #[test]
    fn parse_attachment_kind_round_trips() {
        assert_eq!(parse_attachment_kind("image"), AttachmentKind::Image);
        assert_eq!(parse_attachment_kind("video"), AttachmentKind::Video);
        assert_eq!(parse_attachment_kind("audio"), AttachmentKind::Audio);
        assert_eq!(parse_attachment_kind("document"), AttachmentKind::Document);
        assert_eq!(parse_attachment_kind("unknown"), AttachmentKind::Other);
    }

    #[test]
    fn meta_serde_round_trip() {
        let meta = AttachmentMeta {
            id: "test-id".to_string(),
            conversation_id: Some("conv-123".to_string()),
            kind: "image".to_string(),
            mime_type: "image/png".to_string(),
            file_name: "photo.png".to_string(),
            size: 1024,
            created_at: "2024-01-01T00:00:00Z".to_string(),
        };

        let json = serde_json::to_string(&meta).unwrap();
        let deserialized: AttachmentMeta = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.id, "test-id");
        assert_eq!(deserialized.conversation_id.as_deref(), Some("conv-123"));
        assert_eq!(deserialized.kind, "image");
    }

    #[test]
    fn resolve_to_attachment_returns_none_for_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(resolve_to_attachment(tmp.path(), "nonexistent").is_none());
    }

    #[test]
    fn resolve_to_attachment_reads_and_encodes() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let dir = root.join("data/attachments");
        std::fs::create_dir_all(&dir).unwrap();

        let id = "test-att";
        let data = b"fake image data";
        std::fs::write(dir.join(id), data).unwrap();

        let meta = AttachmentMeta {
            id: id.to_string(),
            conversation_id: None,
            kind: "image".to_string(),
            mime_type: "image/png".to_string(),
            file_name: "test.png".to_string(),
            size: data.len() as u64,
            created_at: "2024-01-01T00:00:00Z".to_string(),
        };
        let meta_json = serde_json::to_string(&meta).unwrap();
        std::fs::write(dir.join(format!("{id}.json")), meta_json).unwrap();

        let attachment = resolve_to_attachment(root, id).unwrap();
        assert!(matches!(attachment.kind, AttachmentKind::Image));
        assert_eq!(attachment.mime_type.as_deref(), Some("image/png"));
        assert_eq!(attachment.file_name.as_deref(), Some("test.png"));
        assert_eq!(attachment.size, Some(data.len() as u64));

        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&attachment.url)
            .unwrap();
        assert_eq!(decoded, data);
    }

    #[test]
    fn delete_conversation_attachments_cleans_up_matching_files() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let dir = root.join("data/attachments");
        std::fs::create_dir_all(&dir).unwrap();

        for (id, conv) in [("a1", "conv-1"), ("a2", "conv-1"), ("a3", "conv-2")] {
            std::fs::write(dir.join(id), b"data").unwrap();
            let meta = serde_json::json!({
                "id": id,
                "conversation_id": conv,
                "kind": "image",
                "mime_type": "image/png",
                "file_name": "f.png",
                "size": 4,
                "created_at": "2024-01-01T00:00:00Z"
            });
            std::fs::write(
                dir.join(format!("{id}.json")),
                serde_json::to_string(&meta).unwrap(),
            )
            .unwrap();
        }

        delete_conversation_attachments(root, "conv-1");

        assert!(!dir.join("a1").exists());
        assert!(!dir.join("a1.json").exists());
        assert!(!dir.join("a2").exists());
        assert!(!dir.join("a2.json").exists());

        assert!(dir.join("a3").exists());
        assert!(dir.join("a3.json").exists());
    }
}
