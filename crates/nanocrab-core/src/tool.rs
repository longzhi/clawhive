use std::collections::HashMap;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use nanocrab_provider::ToolDef;

pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
}

#[async_trait]
pub trait ToolExecutor: Send + Sync {
    fn definition(&self) -> ToolDef;
    async fn execute(&self, input: serde_json::Value) -> Result<ToolOutput>;
}

pub struct ToolRegistry {
    tools: HashMap<String, Box<dyn ToolExecutor>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
        }
    }

    pub fn register(&mut self, tool: Box<dyn ToolExecutor>) {
        let name = tool.definition().name.clone();
        self.tools.insert(name, tool);
    }

    pub fn tool_defs(&self) -> Vec<ToolDef> {
        self.tools.values().map(|t| t.definition()).collect()
    }

    pub async fn execute(&self, name: &str, input: serde_json::Value) -> Result<ToolOutput> {
        let tool = self
            .tools
            .get(name)
            .ok_or_else(|| anyhow!("tool not found: {name}"))?;
        tool.execute(input).await
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct EchoTool;

    #[async_trait]
    impl ToolExecutor for EchoTool {
        fn definition(&self) -> ToolDef {
            ToolDef {
                name: "echo".into(),
                description: "Echo input".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "text": {"type": "string"}
                    },
                    "required": ["text"]
                }),
            }
        }

        async fn execute(&self, input: serde_json::Value) -> Result<ToolOutput> {
            let text = input["text"].as_str().unwrap_or("").to_string();
            Ok(ToolOutput {
                content: text,
                is_error: false,
            })
        }
    }

    #[test]
    fn registry_register_and_list() {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(EchoTool));
        let defs = registry.tool_defs();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "echo");
    }

    #[tokio::test]
    async fn registry_execute_known_tool() {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(EchoTool));
        let result = registry
            .execute("echo", serde_json::json!({"text": "hello"}))
            .await
            .unwrap();
        assert_eq!(result.content, "hello");
        assert!(!result.is_error);
    }

    #[tokio::test]
    async fn registry_execute_unknown_tool() {
        let registry = ToolRegistry::new();
        let result = registry.execute("nonexistent", serde_json::json!({})).await;
        assert!(result.is_err());
    }
}
