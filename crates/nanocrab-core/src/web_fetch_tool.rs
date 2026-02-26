use anyhow::{anyhow, Result};
use async_trait::async_trait;
use nanocrab_provider::ToolDef;

use super::tool::{ToolContext, ToolExecutor, ToolOutput};

const DEFAULT_TIMEOUT_SECS: u64 = 30;
const DEFAULT_MAX_CHARS: usize = 20_000;
const MAX_REDIRECTS: usize = 5;
const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 14_7_2) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/122.0.0.0 Safari/537.36";

pub struct WebFetchTool {
    client: reqwest::Client,
}

impl Default for WebFetchTool {
    fn default() -> Self {
        Self::new()
    }
}

impl WebFetchTool {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(DEFAULT_TIMEOUT_SECS))
            .redirect(reqwest::redirect::Policy::limited(MAX_REDIRECTS))
            .user_agent(USER_AGENT)
            .build()
            .unwrap_or_default();
        Self { client }
    }
}

#[async_trait]
impl ToolExecutor for WebFetchTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "web_fetch".into(),
            description: "Fetch and extract readable content from a URL. Converts HTML to clean markdown text. Use for reading web pages, documentation, articles, or API responses.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "The HTTP or HTTPS URL to fetch"
                    },
                    "max_chars": {
                        "type": "integer",
                        "description": "Maximum characters to return (default: 20000)",
                        "minimum": 100
                    },
                    "extract_mode": {
                        "type": "string",
                        "enum": ["markdown", "text"],
                        "description": "Output format: 'markdown' (default) or 'text' (stripped of all formatting)"
                    }
                },
                "required": ["url"]
            }),
        }
    }

    async fn execute(&self, input: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        use super::policy::HardBaseline;

        let url = input["url"]
            .as_str()
            .ok_or_else(|| anyhow!("missing 'url' field"))?;
        let max_chars = input["max_chars"]
            .as_u64()
            .map(|v| v as usize)
            .unwrap_or(DEFAULT_MAX_CHARS);
        let extract_mode = input["extract_mode"].as_str().unwrap_or("markdown");

        // Validate URL scheme
        if !url.starts_with("http://") && !url.starts_with("https://") {
            return Ok(ToolOutput {
                content: format!(
                    "Invalid URL scheme. Only http:// and https:// are supported: {url}"
                ),
                is_error: true,
            });
        }

        if let Ok(parsed) = reqwest::Url::parse(url) {
            let host = parsed.host_str().unwrap_or("");
            let port = parsed.port_or_known_default().unwrap_or(443);

            // Hard baseline check - SSRF protection regardless of tool origin
            if HardBaseline::network_denied(host, port) {
                return Ok(ToolOutput {
                    content: format!("Network access denied (hard baseline): {host}:{port} - private/internal network blocked"),
                    is_error: true,
                });
            }

            // Policy context check (external skills need network permission)
            if !ctx.check_network(host, port) {
                return Ok(ToolOutput {
                    content: format!("Network access denied for {host}:{port}"),
                    is_error: true,
                });
            }
        }

        // Fetch
        let resp = match self
            .client
            .get(url)
            .header(
                "Accept",
                "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
            )
            .header("Accept-Language", "en-US,en;q=0.9")
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                let msg = if e.is_timeout() {
                    format!("Request timed out after {DEFAULT_TIMEOUT_SECS}s: {url}")
                } else if e.is_connect() {
                    format!("Connection failed: {url} — {e}")
                } else if e.is_redirect() {
                    format!("Too many redirects (>{MAX_REDIRECTS}): {url}")
                } else {
                    format!("Fetch failed: {e}")
                };
                return Ok(ToolOutput {
                    content: msg,
                    is_error: true,
                });
            }
        };

        let status = resp.status();
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_lowercase();

        if !status.is_success() {
            return Ok(ToolOutput {
                content: format!("HTTP {status} fetching {url}"),
                is_error: true,
            });
        }

        let body = match resp.text().await {
            Ok(t) => t,
            Err(e) => {
                return Ok(ToolOutput {
                    content: format!("Failed to read response body: {e}"),
                    is_error: true,
                });
            }
        };

        // Convert based on content type
        let text = if content_type.contains("json") {
            // JSON: return as-is (pretty-print if possible)
            match serde_json::from_str::<serde_json::Value>(&body) {
                Ok(v) => serde_json::to_string_pretty(&v).unwrap_or(body),
                Err(_) => body,
            }
        } else if content_type.contains("html") || body.trim_start().starts_with('<') {
            // HTML: convert to markdown or plain text
            match extract_mode {
                "text" => {
                    let md = htmd::convert(&body).unwrap_or_else(|_| strip_html_tags(&body));
                    strip_markdown(&md)
                }
                _ => {
                    // markdown (default)
                    htmd::convert(&body).unwrap_or_else(|_| strip_html_tags(&body))
                }
            }
        } else {
            // Plain text, XML, etc: return as-is
            body
        };

        let truncated = text.chars().count() > max_chars;
        let output: String = if truncated {
            text.chars().take(max_chars).collect()
        } else {
            text
        };

        // Wrap with metadata
        let mut result = format!("--- web_fetch: {url} ---\n");
        result.push_str(&output);
        if truncated {
            result.push_str(&format!(
                "\n\n[Content truncated at {max_chars} characters. Original content was longer.]"
            ));
        }
        result.push_str("\n--- end ---");

        Ok(ToolOutput {
            content: result,
            is_error: false,
        })
    }
}

/// Simple HTML tag stripper (fallback when htmd fails)
fn strip_html_tags(html: &str) -> String {
    let mut result = String::with_capacity(html.len());
    let mut in_tag = false;
    let mut in_script = false;

    let lower = html.to_lowercase();
    let chars: Vec<char> = html.chars().collect();
    let lower_chars: Vec<char> = lower.chars().collect();

    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '<' {
            if !in_script {
                let remaining: String = lower_chars[i..].iter().collect();
                if remaining.starts_with("<script") || remaining.starts_with("<style") {
                    in_script = true;
                }
            }
            in_tag = true;
        } else if in_tag && chars[i] == '>' {
            if in_script {
                let remaining: String = lower_chars[i.saturating_sub(8)..=i].iter().collect();
                if remaining.contains("</script>") || remaining.contains("</style>") {
                    in_script = false;
                }
            }
            in_tag = false;
        } else if !in_tag && !in_script {
            result.push(chars[i]);
        }
        i += 1;
    }

    // Normalize whitespace
    let mut normalized = String::with_capacity(result.len());
    let mut last_was_newline = false;
    for line in result.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            if !last_was_newline {
                normalized.push('\n');
                last_was_newline = true;
            }
        } else {
            normalized.push_str(trimmed);
            normalized.push('\n');
            last_was_newline = false;
        }
    }

    normalized.trim().to_string()
}

/// Strip markdown formatting to get plain text
fn strip_markdown(md: &str) -> String {
    let mut text = md.to_string();
    // Remove headers
    text = text
        .lines()
        .map(|line| {
            let trimmed = line.trim_start();
            if trimmed.starts_with('#') {
                trimmed.trim_start_matches('#').trim()
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    // Remove bold/italic
    text = text.replace("**", "").replace("__", "");
    text = text.replace('*', "").replace('_', " ");
    // Remove links: [text](url) -> text
    while let Some(start) = text.find('[') {
        if let Some(mid) = text[start..].find("](") {
            if let Some(end) = text[start + mid..].find(')') {
                let link_text = &text[start + 1..start + mid].to_string();
                let full_end = start + mid + end + 1;
                text = format!("{}{}{}", &text[..start], link_text, &text[full_end..]);
                continue;
            }
        }
        break;
    }
    // Remove code blocks
    text = text.replace("```", "");
    text = text.replace('`', "");
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn definition_has_correct_name() {
        let tool = WebFetchTool::new();
        let def = tool.definition();
        assert_eq!(def.name, "web_fetch");
        assert!(def.description.contains("readable content"));
    }

    #[test]
    fn definition_requires_url() {
        let tool = WebFetchTool::new();
        let def = tool.definition();
        let required = def.input_schema["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v.as_str() == Some("url")));
    }

    #[test]
    fn strip_html_basic() {
        let html = "<p>Hello <b>world</b></p>";
        let text = strip_html_tags(html);
        assert_eq!(text, "Hello world");
    }

    #[test]
    fn strip_html_removes_script() {
        let html = "<p>Before</p><script>alert('hi')</script><p>After</p>";
        let text = strip_html_tags(html);
        assert!(text.contains("Before"));
        assert!(text.contains("After"));
        assert!(!text.contains("alert"));
    }

    #[test]
    fn strip_markdown_basic() {
        let md = "# Hello\n\n**bold** and [link](http://example.com)";
        let text = strip_markdown(md);
        assert!(text.contains("Hello"));
        assert!(text.contains("bold"));
        assert!(text.contains("link"));
        assert!(!text.contains("**"));
        assert!(!text.contains("]("));
    }

    #[tokio::test]
    async fn rejects_invalid_scheme() {
        let tool = WebFetchTool::new();
        let ctx = ToolContext::default_policy(std::path::Path::new("/tmp"));
        let result = tool
            .execute(serde_json::json!({"url": "ftp://example.com"}), &ctx)
            .await
            .unwrap();
        assert!(result.is_error);
        assert!(result.content.contains("Invalid URL scheme"));
    }

    #[tokio::test]
    async fn web_fetch_denied_by_policy() {
        let tool = WebFetchTool::new();
        let perms = corral_core::Permissions::builder()
            .network_allow(["api.example.com:443"])
            .build();
        let ctx = ToolContext::new(corral_core::PolicyEngine::new(perms));

        let result = tool
            .execute(serde_json::json!({"url": "https://evil.com/steal"}), &ctx)
            .await
            .unwrap();
        assert!(result.is_error);
        assert!(result.content.contains("denied"));
    }

    #[tokio::test]
    async fn web_fetch_denied_by_external_policy() {
        // External context with no network permissions
        let perms = corral_core::Permissions {
            fs: corral_core::FsPermissions::default(),
            network: corral_core::NetworkPermissions { allow: vec![] },
            exec: vec![],
            env: vec![],
            services: Default::default(),
        };
        let ctx = ToolContext::external(perms);

        let tool = WebFetchTool::new();
        let result = tool
            .execute(serde_json::json!({"url": "https://example.com"}), &ctx)
            .await
            .unwrap();
        assert!(result.is_error);
        assert!(result.content.contains("denied"));
    }

    #[tokio::test]
    async fn web_fetch_allowed_by_builtin() {
        // Builtin context allows network (but we can't actually fetch in tests)
        // Just verify it doesn't immediately deny
        let ctx = ToolContext::builtin();
        let tool = WebFetchTool::new();

        // Use a non-routable IP to avoid actual network call
        let result = tool
            .execute(
                serde_json::json!({"url": "https://10.255.255.1/test"}),
                &ctx,
            )
            .await
            .unwrap();
        // Should be denied by hard baseline (private network), not by policy
        assert!(result.is_error);
        assert!(result.content.contains("hard baseline"));
    }
}
