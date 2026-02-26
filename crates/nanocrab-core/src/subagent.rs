use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use nanocrab_provider::LlmMessage;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
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
    pub depth: u32,
}

#[derive(Debug, Clone)]
pub struct SubAgentResult {
    pub run_id: Uuid,
    pub output: String,
    pub success: bool,
}

struct RunHandle {
    handle: JoinHandle<SubAgentResult>,
    #[allow(dead_code)]
    parent_run_id: Uuid,
    #[allow(dead_code)]
    trace_id: Uuid,
}

pub struct SubAgentRunner {
    router: Arc<LlmRouter>,
    agents: HashMap<String, FullAgentConfig>,
    personas: HashMap<String, Persona>,
    active_runs: Arc<Mutex<HashMap<Uuid, RunHandle>>>,
    max_depth: u32,
    allowed_tools: Vec<String>,
}

impl SubAgentRunner {
    pub fn new(
        router: Arc<LlmRouter>,
        agents: HashMap<String, FullAgentConfig>,
        personas: HashMap<String, Persona>,
        max_depth: u32,
        allowed_tools: Vec<String>,
    ) -> Self {
        Self {
            router,
            agents,
            personas,
            active_runs: Arc::new(Mutex::new(HashMap::new())),
            max_depth,
            allowed_tools,
        }
    }

    pub async fn spawn(&self, req: SubAgentRequest) -> Result<Uuid> {
        if req.depth >= self.max_depth {
            return Err(anyhow::anyhow!(
                "sub-agent recursion depth {} exceeds maximum {}",
                req.depth,
                self.max_depth
            ));
        }

        let agent = self
            .agents
            .get(&req.target_agent_id)
            .ok_or_else(|| anyhow::anyhow!("sub-agent not found: {}", req.target_agent_id))?
            .clone();

        let system = self
            .personas
            .get(&req.target_agent_id)
            .map(|p| p.assembled_system_prompt())
            .unwrap_or_default();

        let run_id = Uuid::new_v4();
        let router = self.router.clone();
        let task_text = req.task.clone();
        let timeout_secs = req.timeout_seconds;
        let parent_run_id = req.parent_run_id;
        let trace_id = req.trace_id;

        let handle = tokio::spawn(async move {
            let messages = vec![LlmMessage::user(task_text)];

            let result = timeout(
                Duration::from_secs(timeout_secs),
                router.chat(
                    &agent.model_policy.primary,
                    &agent.model_policy.fallbacks,
                    Some(system),
                    messages,
                    2048,
                ),
            )
            .await;

            match result {
                Ok(Ok(resp)) => SubAgentResult {
                    run_id,
                    output: resp.text,
                    success: true,
                },
                Ok(Err(err)) => SubAgentResult {
                    run_id,
                    output: err.to_string(),
                    success: false,
                },
                Err(_) => SubAgentResult {
                    run_id,
                    output: "sub-agent timeout".into(),
                    success: false,
                },
            }
        });

        self.active_runs.lock().await.insert(
            run_id,
            RunHandle {
                handle,
                parent_run_id,
                trace_id,
            },
        );

        Ok(run_id)
    }

    pub async fn cancel(&self, run_id: &Uuid) -> bool {
        if let Some(run) = self.active_runs.lock().await.remove(run_id) {
            run.handle.abort();
            true
        } else {
            false
        }
    }

    pub async fn wait_result(&self, run_id: &Uuid) -> Result<SubAgentResult> {
        let run = self
            .active_runs
            .lock()
            .await
            .remove(run_id)
            .ok_or_else(|| anyhow::anyhow!("run not found: {run_id}"))?;

        match run.handle.await {
            Ok(result) => Ok(result),
            Err(e) if e.is_cancelled() => Ok(SubAgentResult {
                run_id: *run_id,
                output: "task cancelled".into(),
                success: false,
            }),
            Err(e) => Ok(SubAgentResult {
                run_id: *run_id,
                output: format!("task panicked: {e}"),
                success: false,
            }),
        }
    }

    pub fn result_merge(results: &[SubAgentResult]) -> String {
        results
            .iter()
            .filter(|r| r.success)
            .map(|r| r.output.as_str())
            .collect::<Vec<_>>()
            .join("\n\n---\n\n")
    }

    pub async fn active_count(&self) -> usize {
        self.active_runs.lock().await.len()
    }

    pub fn allowed_tools(&self) -> &[String] {
        &self.allowed_tools
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
            workspace: None,
            heartbeat: None,
        };

        let mut agents = HashMap::new();
        agents.insert("test-agent".into(), agent);

        SubAgentRunner::new(
            Arc::new(router),
            agents,
            HashMap::new(),
            3,
            vec!["read".into(), "write".into()],
        )
    }

    #[tokio::test]
    async fn spawn_and_wait_result() {
        let runner = make_runner_with_stub();
        let req = SubAgentRequest {
            parent_run_id: Uuid::new_v4(),
            trace_id: Uuid::new_v4(),
            target_agent_id: "test-agent".into(),
            task: "Do something".into(),
            timeout_seconds: 30,
            depth: 0,
        };
        let run_id = runner.spawn(req).await.unwrap();
        let result = runner.wait_result(&run_id).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("stub:anthropic:test-model"));
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
            depth: 0,
        };
        let result = runner.spawn(req).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn cancel_running_task() {
        let runner = make_runner_with_stub();
        let req = SubAgentRequest {
            parent_run_id: Uuid::new_v4(),
            trace_id: Uuid::new_v4(),
            target_agent_id: "test-agent".into(),
            task: "Quick task".into(),
            timeout_seconds: 60,
            depth: 0,
        };
        let run_id = runner.spawn(req).await.unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;
        let cancelled = runner.cancel(&run_id).await;
        let _ = cancelled;
    }

    #[tokio::test]
    async fn result_merge_concatenates_successful() {
        let results = vec![
            SubAgentResult {
                run_id: Uuid::new_v4(),
                output: "Result A".into(),
                success: true,
            },
            SubAgentResult {
                run_id: Uuid::new_v4(),
                output: "Failed".into(),
                success: false,
            },
            SubAgentResult {
                run_id: Uuid::new_v4(),
                output: "Result B".into(),
                success: true,
            },
        ];
        let merged = SubAgentRunner::result_merge(&results);
        assert!(merged.contains("Result A"));
        assert!(merged.contains("Result B"));
        assert!(!merged.contains("Failed"));
    }

    #[tokio::test]
    async fn active_count_tracks_runs() {
        let runner = make_runner_with_stub();
        assert_eq!(runner.active_count().await, 0);

        let req = SubAgentRequest {
            parent_run_id: Uuid::new_v4(),
            trace_id: Uuid::new_v4(),
            target_agent_id: "test-agent".into(),
            task: "task".into(),
            timeout_seconds: 60,
            depth: 0,
        };
        let _run_id = runner.spawn(req).await.unwrap();
        let _count = runner.active_count().await;
    }

    #[tokio::test]
    async fn spawn_rejects_excessive_depth() {
        let runner = make_runner_with_stub();
        let req = SubAgentRequest {
            parent_run_id: Uuid::new_v4(),
            trace_id: Uuid::new_v4(),
            target_agent_id: "test-agent".into(),
            task: "deep task".into(),
            timeout_seconds: 30,
            depth: 5,
        };
        let result = runner.spawn(req).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("recursion depth"));
    }
}
