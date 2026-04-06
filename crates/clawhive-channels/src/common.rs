pub const PROGRESS_MESSAGE: &str = "⏳ Still working on it... (send /stop to cancel)";

/// Infer a MIME type from a filename extension when the channel API omits it.
pub fn infer_mime_from_filename(name: Option<&str>) -> Option<String> {
    let ext = name?.rsplit('.').next()?;
    let mime = match ext.to_ascii_lowercase().as_str() {
        "txt" | "log" => "text/plain",
        "md" | "markdown" => "text/markdown",
        "json" => "application/json",
        "yaml" | "yml" => "application/x-yaml",
        "toml" => "application/toml",
        "xml" => "application/xml",
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "js" | "mjs" => "application/javascript",
        "ts" | "tsx" => "text/typescript",
        "py" => "text/x-python",
        "rs" => "text/x-rust",
        "go" => "text/x-go",
        "java" => "text/x-java",
        "c" | "h" => "text/x-c",
        "cpp" | "cc" | "cxx" | "hpp" => "text/x-c++",
        "sh" | "bash" | "zsh" => "application/x-sh",
        "csv" => "text/csv",
        "sql" => "text/x-sql",
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "mp4" => "video/mp4",
        "mp3" => "audio/mpeg",
        "ogg" | "oga" => "audio/ogg",
        "wav" => "audio/wav",
        _ => return None,
    };
    Some(mime.to_string())
}

pub struct AbortOnDrop(pub tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[cfg(test)]
mod tests {
    use std::future::pending;

    use super::{infer_mime_from_filename, AbortOnDrop};

    #[tokio::test]
    async fn abort_on_drop_aborts_task() {
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let guard = AbortOnDrop(tokio::spawn(async move {
            let _tx = tx;
            pending::<()>().await;
        }));

        drop(guard);

        assert!(rx.await.is_err());
    }

    #[test]
    fn infer_mime_from_filename_handles_pdf_and_text_extensions() {
        assert_eq!(
            infer_mime_from_filename(Some("lease.PDF")),
            Some("application/pdf".to_string())
        );
        assert_eq!(
            infer_mime_from_filename(Some("notes.txt")),
            Some("text/plain".to_string())
        );
        assert_eq!(infer_mime_from_filename(Some("archive.bin")), None);
    }
}
