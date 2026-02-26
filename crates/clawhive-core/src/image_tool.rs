//! Image analysis tool using vision-capable models.
//!
//! Accepts image URLs or base64 data and sends them to a vision model
//! for analysis/description.

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use clawhive_provider::ToolDef;
use serde_json::json;

use super::tool::{ToolContext, ToolExecutor, ToolOutput};

const DEFAULT_MAX_TOKENS: u32 = 1024;

pub struct ImageTool;

impl Default for ImageTool {
    fn default() -> Self {
        Self::new()
    }
}

impl ImageTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ToolExecutor for ImageTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "image".to_string(),
            description: "Analyze an image using a vision model. Accepts image URL or base64 data URI.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "image": {
                        "type": "string",
                        "description": "Image URL (https://...) or base64 data URI (data:image/...;base64,...)"
                    },
                    "images": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Multiple image URLs or data URIs (up to 10)"
                    },
                    "prompt": {
                        "type": "string",
                        "description": "What to analyze or describe about the image(s). Default: 'Describe this image.'"
                    },
                    "max_tokens": {
                        "type": "integer",
                        "description": "Maximum tokens for the response"
                    }
                },
                "required": []
            }),
        }
    }

    async fn execute(&self, input: serde_json::Value, _ctx: &ToolContext) -> Result<ToolOutput> {
        // Collect image URLs
        let mut image_urls: Vec<String> = vec![];

        if let Some(single) = input.get("image").and_then(|v| v.as_str()) {
            image_urls.push(single.to_string());
        }

        if let Some(arr) = input.get("images").and_then(|v| v.as_array()) {
            for item in arr {
                if let Some(url) = item.as_str() {
                    image_urls.push(url.to_string());
                }
            }
        }

        if image_urls.is_empty() {
            return Err(anyhow!("No image provided. Use 'image' or 'images' parameter."));
        }

        if image_urls.len() > 10 {
            return Err(anyhow!("Too many images. Maximum is 10."));
        }

        let prompt = input
            .get("prompt")
            .and_then(|v| v.as_str())
            .unwrap_or("Describe this image.");

        let _max_tokens = input
            .get("max_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_MAX_TOKENS as u64) as u32;

        // Build content blocks for the vision request
        let mut content_parts = vec![];

        for url in &image_urls {
            if url.starts_with("data:") {
                // Base64 data URI
                content_parts.push(json!({
                    "type": "image_url",
                    "image_url": { "url": url }
                }));
            } else if url.starts_with("http://") || url.starts_with("https://") {
                content_parts.push(json!({
                    "type": "image_url",
                    "image_url": { "url": url }
                }));
            } else {
                return Err(anyhow!("Invalid image URL: {url}. Must be http(s):// or data: URI."));
            }
        }

        // Add text prompt
        content_parts.push(json!({
            "type": "text",
            "text": prompt
        }));

        // Return the analysis request info
        // The actual vision API call would be handled by the orchestrator
        // which has access to the provider registry
        let result = json!({
            "status": "ready",
            "images": image_urls.len(),
            "prompt": prompt,
            "content": content_parts,
        });

        Ok(ToolOutput {
            content: serde_json::to_string_pretty(&result)?,
            is_error: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn definition_has_correct_name() {
        let tool = ImageTool::new();
        let def = tool.definition();
        assert_eq!(def.name, "image");
    }

    #[tokio::test]
    async fn rejects_empty_input() {
        let tool = ImageTool::new();
        let ctx = ToolContext::builtin();
        let result = tool.execute(json!({}), &ctx).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn accepts_single_image_url() {
        let tool = ImageTool::new();
        let ctx = ToolContext::builtin();
        let result = tool
            .execute(
                json!({
                    "image": "https://example.com/photo.jpg",
                    "prompt": "What is in this image?"
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(!result.is_error);
        assert!(result.content.contains("ready"));
    }

    #[tokio::test]
    async fn accepts_multiple_images() {
        let tool = ImageTool::new();
        let ctx = ToolContext::builtin();
        let result = tool
            .execute(
                json!({
                    "images": [
                        "https://example.com/a.jpg",
                        "https://example.com/b.jpg"
                    ]
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(!result.is_error);
        assert!(result.content.contains("\"images\": 2"));
    }

    #[tokio::test]
    async fn rejects_too_many_images() {
        let tool = ImageTool::new();
        let ctx = ToolContext::builtin();
        let urls: Vec<String> = (0..11).map(|i| format!("https://example.com/{i}.jpg")).collect();
        let result = tool.execute(json!({ "images": urls }), &ctx).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn rejects_invalid_url() {
        let tool = ImageTool::new();
        let ctx = ToolContext::builtin();
        let result = tool
            .execute(json!({ "image": "/local/path.jpg" }), &ctx)
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn accepts_data_uri() {
        let tool = ImageTool::new();
        let ctx = ToolContext::builtin();
        let result = tool
            .execute(json!({ "image": "data:image/png;base64,iVBORw0KGgo=" }), &ctx)
            .await
            .unwrap();
        assert!(!result.is_error);
    }
}
