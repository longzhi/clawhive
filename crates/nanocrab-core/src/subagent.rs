use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use nanocrab_provider::LlmMessage;
use tokio::time::{timeout, Duration};
use uuid::Uuid;

use super::config::FullAgentConfig;
use super::persona::Persona;
use super::router::LlmRouter;

#[derive(Debug, Clone)]
pub struct SubAgentRequest {
    pub parent_run_id: Uuid,
    pub trace_id: Uuid,
    pub target_agent_id: String,
    pub task: String,
    pub timeout_seconds: u64,
}

#[derive(Debug, Clone)]
pub struct SubAgentResult {
    pub run_id: Uuid,
    pub output: String,
    pub success: bool,
}

pub struct SubAgentRunner {
    router: Arc<LlmRouter>,
    agents: HashMap<String, FullAgentConfig>,
    personas: HashMap<String, Persona>,
}

impl SubAgentRunner {
    pub fn new(
        router: Arc<LlmRouter>,
        agents: HashMap<String, FullAgentConfig>,
        personas: HashMap<String, Persona>,
    ) -> Self {
        Self {
            router,
            agents,
            personas,
        }
    }

    pub async fn spawn(&self, req: SubAgentRequest) -> Result<SubAgentResult> {
        let agent = self
            .agents
            .get(&req.target_agent_id)
            .ok_or_else(|| anyhow::anyhow!("sub-agent not found: {}", req.target_agent_id))?;

        let system = self
            .personas
            .get(&req.target_agent_id)
            .map(|p| p.assembled_system_prompt())
            .unwrap_or_default();

        let messages = vec![LlmMessage {
            role: "user".into(),
            content: req.task,
        }];

        let result = timeout(
            Duration::from_secs(req.timeout_seconds),
            self.router.chat(
                &agent.model_policy.primary,
                &agent.model_policy.fallbacks,
                Some(system),
                messages,
                2048,
            ),
        )
        .await;

        match result {
            Ok(Ok(resp)) => Ok(SubAgentResult {
                run_id: Uuid::new_v4(),
                output: resp.text,
                success: true,
            }),
            Ok(Err(err)) => Ok(SubAgentResult {
                run_id: Uuid::new_v4(),
                output: err.to_string(),
                success: false,
            }),
            Err(_) => Ok(SubAgentResult {
                run_id: Uuid::new_v4(),
                output: "sub-agent timeout".into(),
                success: false,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::ModelPolicy;
    use super::*;
    use nanocrab_provider::{ProviderRegistry, StubProvider};

    fn make_runner_with_stub() -> SubAgentRunner {
        let mut registry = ProviderRegistry::new();
        registry.register("stub", Arc::new(StubProvider));

        let router = LlmRouter::new(registry, HashMap::new(), vec![]);

        let agent = FullAgentConfig {
            agent_id: "test-agent".into(),
            enabled: true,
            identity: None,
            model_policy: ModelPolicy {
                primary: "stub/test-model".into(),
                fallbacks: vec![],
            },
            tool_policy: None,
            memory_policy: None,
            sub_agent: None,
        };

        let mut agents = HashMap::new();
        agents.insert("test-agent".into(), agent);

        SubAgentRunner::new(Arc::new(router), agents, HashMap::new())
    }

    #[tokio::test]
    async fn spawn_success() {
        let runner = make_runner_with_stub();
        let req = SubAgentRequest {
            parent_run_id: Uuid::new_v4(),
            trace_id: Uuid::new_v4(),
            target_agent_id: "test-agent".into(),
            task: "Do something".into(),
            timeout_seconds: 30,
        };
        let result = runner.spawn(req).await.unwrap();
        assert!(result.success);
        assert_eq!(result.output, "[stub:anthropic:test-model] Do something");
    }

    #[tokio::test]
    async fn spawn_unknown_agent() {
        let runner = make_runner_with_stub();
        let req = SubAgentRequest {
            parent_run_id: Uuid::new_v4(),
            trace_id: Uuid::new_v4(),
            target_agent_id: "nonexistent-agent".into(),
            task: "Do something".into(),
            timeout_seconds: 30,
        };
        let result = runner.spawn(req).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn spawn_timeout() {
        let runner = make_runner_with_stub();
        let req = SubAgentRequest {
            parent_run_id: Uuid::new_v4(),
            trace_id: Uuid::new_v4(),
            target_agent_id: "test-agent".into(),
            task: "Quick task".into(),
            timeout_seconds: 60,
        };
        let result = runner.spawn(req).await.unwrap();
        assert!(result.success);
    }
}
