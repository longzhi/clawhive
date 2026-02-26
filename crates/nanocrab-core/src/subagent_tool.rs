use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use nanocrab_provider::ToolDef;
use uuid::Uuid;

use super::subagent::{SubAgentRequest, SubAgentRunner};
use super::tool::{ToolContext, ToolExecutor, ToolOutput};

pub struct SubAgentTool {
    runner: Arc<SubAgentRunner>,
    default_timeout: u64,
}

impl SubAgentTool {
    pub fn new(runner: Arc<SubAgentRunner>, default_timeout: u64) -> Self {
        Self {
            runner,
            default_timeout,
        }
    }
}

#[async_trait]
impl ToolExecutor for SubAgentTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "delegate_task".into(),
            description: "Delegate a task to a sub-agent. The sub-agent runs independently with its own persona and returns a result.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "target_agent_id": {
                        "type": "string",
                        "description": "The ID of the agent to delegate to"
                    },
                    "task": {
                        "type": "string",
                        "description": "The task description for the sub-agent"
                    },
                    "timeout_seconds": {
                        "type": "integer",
                        "description": "Timeout in seconds (default: 30)",
                        "default": 30
                    }
                },
                "required": ["target_agent_id", "task"]
            }),
        }
    }

    async fn execute(&self, input: serde_json::Value, _ctx: &ToolContext) -> Result<ToolOutput> {
        let target_agent_id = input["target_agent_id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("missing 'target_agent_id' field"))?
            .to_string();

        let task = input["task"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("missing 'task' field"))?
            .to_string();

        let timeout_seconds = input["timeout_seconds"]
            .as_u64()
            .unwrap_or(self.default_timeout);

        let req = SubAgentRequest {
            parent_run_id: Uuid::new_v4(),
            trace_id: Uuid::new_v4(),
            target_agent_id,
            task,
            timeout_seconds,
            depth: 0,
        };

        let run_id = match self.runner.spawn(req).await {
            Ok(id) => id,
            Err(e) => {
                return Ok(ToolOutput {
                    content: format!("Failed to spawn sub-agent: {e}"),
                    is_error: true,
                });
            }
        };

        match self.runner.wait_result(&run_id).await {
            Ok(result) => Ok(ToolOutput {
                content: result.output,
                is_error: !result.success,
            }),
            Err(e) => Ok(ToolOutput {
                content: format!("Failed to get sub-agent result: {e}"),
                is_error: true,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FullAgentConfig, ModelPolicy};
    use nanocrab_provider::{ProviderRegistry, StubProvider};
    use std::collections::HashMap;

    fn make_sub_agent_tool() -> SubAgentTool {
        let mut registry = ProviderRegistry::new();
        registry.register("stub", Arc::new(StubProvider));

        let router = crate::LlmRouter::new(registry, HashMap::new(), vec![]);

        let agent = FullAgentConfig {
            agent_id: "helper".into(),
            enabled: true,
            identity: None,
            model_policy: ModelPolicy {
                primary: "stub/test-model".into(),
                fallbacks: vec![],
            },
            tool_policy: None,
            memory_policy: None,
            sub_agent: None,
            workspace: None,
            heartbeat: None,
        };

        let mut agents = HashMap::new();
        agents.insert("helper".into(), agent);

        let runner = Arc::new(crate::SubAgentRunner::new(
            Arc::new(router),
            agents,
            HashMap::new(),
            3,
            vec![],
        ));

        SubAgentTool::new(runner, 30)
    }

    #[test]
    fn tool_definition_is_correct() {
        let tool = make_sub_agent_tool();
        let def = tool.definition();
        assert_eq!(def.name, "delegate_task");
        assert!(def.input_schema["properties"]["target_agent_id"].is_object());
        assert!(def.input_schema["properties"]["task"].is_object());
    }

    #[tokio::test]
    async fn delegate_to_valid_agent() {
        let tool = make_sub_agent_tool();
        let ctx = ToolContext::default_policy(std::path::Path::new("/tmp"));
        let result = tool
            .execute(
                serde_json::json!({
                    "target_agent_id": "helper",
                    "task": "Say hello"
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!result.is_error);
        assert!(result.content.contains("stub:anthropic:test-model"));
    }

    #[tokio::test]
    async fn delegate_to_unknown_agent() {
        let tool = make_sub_agent_tool();
        let ctx = ToolContext::default_policy(std::path::Path::new("/tmp"));
        let result = tool
            .execute(
                serde_json::json!({
                    "target_agent_id": "nonexistent",
                    "task": "Do something"
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(result.is_error);
        assert!(result.content.contains("Failed to spawn"));
    }

    #[tokio::test]
    async fn missing_required_field() {
        let tool = make_sub_agent_tool();
        let ctx = ToolContext::default_policy(std::path::Path::new("/tmp"));
        let result = tool
            .execute(
                serde_json::json!({
                    "target_agent_id": "helper"
                }),
                &ctx,
            )
            .await;
        assert!(result.is_err());
    }
}
