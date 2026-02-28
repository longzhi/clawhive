use axum::http::{header, StatusCode, Uri};
use axum::response::{Html, IntoResponse, Response};
use rust_embed::Embed;

#[derive(Embed)]
#[folder = "../../web/dist"]
struct Assets;

pub async fn frontend_handler(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');

    // Try exact file match first
    if let Some(content) = Assets::get(path) {
        let mime = mime_guess::from_path(path).first_or_octet_stream();

        let cache = if path.starts_with("assets/") {
            // Vite hashed assets — cache for 1 year
            "public, max-age=31536000, immutable"
        } else {
            // Other static files — short cache
            "public, max-age=60"
        };

        return (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, mime.as_ref().to_string()),
                (header::CACHE_CONTROL, cache.to_string()),
            ],
            content.data.into_response(),
        )
            .into_response();
    }

    // SPA fallback: serve index.html for non-file paths
    match Assets::get("index.html") {
        Some(content) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "text/html".to_string()),
                (
                    header::CACHE_CONTROL,
                    "no-cache, no-store, must-revalidate".to_string(),
                ),
            ],
            content.data.into_response(),
        )
            .into_response(),
        None => Html(
            "<h1>Frontend not built</h1><p>Run <code>cd web &amp;&amp; bun install &amp;&amp; bun run build</code> and rebuild the server.</p>",
        )
        .into_response(),
    }
}
