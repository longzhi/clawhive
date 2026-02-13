use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use nanocrab_bus::BusPublisher;
use nanocrab_memory::file_store::MemoryFileStore;
use nanocrab_memory::SessionWriter;
use nanocrab_memory::{Episode, MemoryStore};
use nanocrab_provider::LlmMessage;
use nanocrab_runtime::TaskExecutor;
use nanocrab_schema::*;

use super::config::FullAgentConfig;
use super::persona::Persona;
use super::router::LlmRouter;
use super::session::SessionManager;
use super::skill::SkillRegistry;

pub struct Orchestrator {
    router: LlmRouter,
    agents: HashMap<String, FullAgentConfig>,
    personas: HashMap<String, Persona>,
    session_mgr: SessionManager,
    skill_registry: SkillRegistry,
    memory: Arc<MemoryStore>,
    bus: BusPublisher,
    runtime: Arc<dyn TaskExecutor>,
    file_store: MemoryFileStore,
    session_writer: SessionWriter,
    react_max_steps: usize,
    react_repeat_guard: usize,
}

impl Orchestrator {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        router: LlmRouter,
        agents: Vec<FullAgentConfig>,
        personas: HashMap<String, Persona>,
        session_mgr: SessionManager,
        skill_registry: SkillRegistry,
        memory: Arc<MemoryStore>,
        bus: BusPublisher,
        runtime: Arc<dyn TaskExecutor>,
        file_store: MemoryFileStore,
        session_writer: SessionWriter,
    ) -> Self {
        let agents_map = agents
            .into_iter()
            .map(|a| (a.agent_id.clone(), a))
            .collect();
        Self {
            router,
            agents: agents_map,
            personas,
            session_mgr,
            skill_registry,
            memory,
            bus,
            runtime,
            file_store,
            session_writer,
            react_max_steps: 4,
            react_repeat_guard: 2,
        }
    }

    pub fn with_react_config(mut self, react: super::WeakReActConfig) -> Self {
        self.react_max_steps = react.max_steps;
        self.react_repeat_guard = react.repeat_guard;
        self
    }

    pub async fn handle_inbound(
        &self,
        inbound: InboundMessage,
        agent_id: &str,
    ) -> Result<OutboundMessage> {
        let agent = self
            .agents
            .get(agent_id)
            .ok_or_else(|| anyhow!("agent not found: {agent_id}"))?;

        let session_key = SessionKey::from_inbound(&inbound);
        let _session = self
            .session_mgr
            .get_or_create(&session_key, agent_id)
            .await?;

        // Save inbound data before it's moved
        let inbound_at = inbound.at;
        let inbound_text = inbound.text.clone();

        let system_prompt = self
            .personas
            .get(agent_id)
            .map(|p| p.assembled_system_prompt())
            .unwrap_or_default();
        let skill_summary = self.skill_registry.summary_prompt();
        let system_prompt = if skill_summary.is_empty() {
            system_prompt
        } else {
            format!("{system_prompt}\n\n{skill_summary}")
        };

        let memory_context = self
            .build_memory_context(&session_key, &inbound.text)
            .await?;

        let mut messages = Vec::new();
        if !memory_context.is_empty() {
            messages.push(LlmMessage {
                role: "user".into(),
                content: format!("[memory context]\n{memory_context}"),
            });
            messages.push(LlmMessage {
                role: "assistant".into(),
                content: "Understood, I have the context.".into(),
            });
        }
        messages.push(LlmMessage {
            role: "user".into(),
            content: self.runtime.execute(&inbound.text).await?,
        });

        let reply_text = self
            .weak_react_loop(
                &agent.model_policy.primary,
                &agent.model_policy.fallbacks,
                Some(system_prompt),
                messages,
            )
            .await?;
        let reply_text = self.runtime.execute(&reply_text).await?;

        let outbound = OutboundMessage {
            trace_id: inbound.trace_id,
            channel_type: inbound.channel_type.clone(),
            connector_id: inbound.connector_id.clone(),
            conversation_scope: inbound.conversation_scope.clone(),
            text: reply_text,
            at: chrono::Utc::now(),
        };

        // Record episodes
        let user_ep = Episode {
            id: uuid::Uuid::new_v4(),
            ts: inbound_at,
            session_id: session_key.0.clone(),
            speaker: "user".into(),
            text: inbound_text.clone(),
            tags: vec![],
            importance: 0.5,
            context_hash: None,
            source_ref: None,
        };
        if let Err(e) = self.memory.insert_episode(user_ep).await {
            tracing::warn!("Failed to record user episode: {e}");
        }
        if let Err(e) = self
            .session_writer
            .append_message(&session_key.0, "user", &inbound_text)
            .await
        {
            tracing::warn!("Failed to write user session entry: {e}");
        }

        let asst_ep = Episode {
            id: uuid::Uuid::new_v4(),
            ts: outbound.at,
            session_id: session_key.0.clone(),
            speaker: agent_id.to_string(),
            text: outbound.text.clone(),
            tags: vec![],
            importance: 0.5,
            context_hash: None,
            source_ref: None,
        };
        if let Err(e) = self.memory.insert_episode(asst_ep).await {
            tracing::warn!("Failed to record assistant episode: {e}");
        }
        if let Err(e) = self
            .session_writer
            .append_message(&session_key.0, agent_id, &outbound.text)
            .await
        {
            tracing::warn!("Failed to write assistant session entry: {e}");
        }

        let _ = self
            .bus
            .publish(BusMessage::ReplyReady {
                outbound: outbound.clone(),
            })
            .await;

        Ok(outbound)
    }

    async fn weak_react_loop(
        &self,
        primary: &str,
        fallbacks: &[String],
        system: Option<String>,
        initial_messages: Vec<LlmMessage>,
    ) -> Result<String> {
        let mut messages = initial_messages;
        let mut repeated = 0usize;
        let mut last_reply = String::new();

        for _step in 0..self.react_max_steps {
            let resp = self
                .router
                .chat(primary, fallbacks, system.clone(), messages.clone(), 2048)
                .await?;
            let reply = resp.text;

            if reply == last_reply {
                repeated += 1;
                if repeated >= self.react_repeat_guard {
                    return Ok(format!("{reply}\n[weak-react: stopped, repeated]"));
                }
            } else {
                repeated = 0;
            }

            if reply.contains("[finish]") {
                return Ok(reply.replace("[finish]", "").trim().to_string());
            }

            let has_continuation = reply.contains("[think]") || reply.contains("[action]");
            if !has_continuation {
                return Ok(reply);
            }

            last_reply = reply.clone();
            messages.push(LlmMessage {
                role: "assistant".into(),
                content: reply,
            });
        }

        Ok(last_reply)
    }

    async fn build_memory_context(
        &self,
        _session_key: &SessionKey,
        _query: &str,
    ) -> Result<String> {
        self.file_store.build_memory_context().await
    }
}
