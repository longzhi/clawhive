use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use futures_core::Stream;
use nanocrab_bus::BusPublisher;
use nanocrab_memory::embedding::EmbeddingProvider;
use nanocrab_memory::file_store::MemoryFileStore;
use nanocrab_memory::search_index::SearchIndex;
use nanocrab_memory::{Episode, MemoryStore};
use nanocrab_memory::{SessionReader, SessionWriter};
use nanocrab_provider::{ContentBlock, LlmMessage, LlmRequest, StreamChunk};
use nanocrab_runtime::TaskExecutor;
use nanocrab_schema::*;

use super::config::FullAgentConfig;
use super::file_tools::{EditFileTool, ReadFileTool, WriteFileTool};
use super::memory_tools::{MemoryGetTool, MemorySearchTool};
use super::persona::Persona;
use super::router::LlmRouter;
use super::schedule_tool::ScheduleTool;
use super::session::SessionManager;
use super::shell_tool::ExecuteCommandTool;
use super::skill::SkillRegistry;
use super::tool::{ConversationMessage, ToolContext, ToolExecutor, ToolRegistry};
use super::web_fetch_tool::WebFetchTool;
use super::web_search_tool::WebSearchTool;
use super::workspace::Workspace;

/// Per-agent workspace runtime state: file store, session I/O, search index.
struct AgentWorkspaceState {
    workspace: Workspace,
    file_store: MemoryFileStore,
    session_writer: SessionWriter,
    session_reader: SessionReader,
    search_index: SearchIndex,
}

pub struct Orchestrator {
    router: Arc<LlmRouter>,
    agents: HashMap<String, FullAgentConfig>,
    personas: HashMap<String, Persona>,
    session_mgr: SessionManager,
    skill_registry: SkillRegistry,
    skills_root: std::path::PathBuf,
    memory: Arc<MemoryStore>,
    bus: BusPublisher,
    runtime: Arc<dyn TaskExecutor>,
    workspace_root: std::path::PathBuf,
    /// Per-agent workspace state, keyed by agent_id
    agent_workspaces: HashMap<String, AgentWorkspaceState>,
    /// Fallback for agents without a dedicated workspace
    file_store: MemoryFileStore,
    session_writer: SessionWriter,
    session_reader: SessionReader,
    search_index: SearchIndex,
    embedding_provider: Arc<dyn EmbeddingProvider>,
    tool_registry: ToolRegistry,
    default_workspace_root: std::path::PathBuf,
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
        session_reader: SessionReader,
        search_index: SearchIndex,
        embedding_provider: Arc<dyn EmbeddingProvider>,
        workspace_root: std::path::PathBuf,
        brave_api_key: Option<String>,
        project_root: Option<std::path::PathBuf>,
        schedule_manager: Arc<nanocrab_scheduler::ScheduleManager>,
    ) -> Self {
        let router = Arc::new(router);
        let agents_map: HashMap<String, FullAgentConfig> = agents
            .into_iter()
            .map(|a| (a.agent_id.clone(), a))
            .collect();
        let personas_for_subagent = personas.clone();

        // Build per-agent workspace states
        let effective_project_root = project_root.unwrap_or_else(|| workspace_root.clone());
        let mut agent_workspaces = HashMap::new();
        for (agent_id, agent_cfg) in &agents_map {
            let ws = Workspace::resolve(
                &effective_project_root,
                agent_id,
                agent_cfg.workspace.as_deref(),
            );
            let ws_root = ws.root().to_path_buf();
            let state = AgentWorkspaceState {
                workspace: ws,
                file_store: MemoryFileStore::new(&ws_root),
                session_writer: SessionWriter::new(&ws_root),
                session_reader: SessionReader::new(&ws_root),
                search_index: SearchIndex::new(memory.db()),
            };
            agent_workspaces.insert(agent_id.clone(), state);
        }

        let mut tool_registry = ToolRegistry::new();
        tool_registry.register(Box::new(MemorySearchTool::new(
            search_index.clone(),
            embedding_provider.clone(),
        )));
        tool_registry.register(Box::new(MemoryGetTool::new(file_store.clone())));
        let sub_agent_runner = Arc::new(super::subagent::SubAgentRunner::new(
            router.clone(),
            agents_map.clone(),
            personas_for_subagent,
            3,
            vec![],
        ));
        tool_registry.register(Box::new(super::subagent_tool::SubAgentTool::new(
            sub_agent_runner,
            30,
        )));
        tool_registry.register(Box::new(ReadFileTool::new(workspace_root.clone())));
        tool_registry.register(Box::new(WriteFileTool::new(workspace_root.clone())));
        tool_registry.register(Box::new(EditFileTool::new(workspace_root.clone())));
        tool_registry.register(Box::new(ExecuteCommandTool::new(workspace_root.clone(), 30)));
        tool_registry.register(Box::new(WebFetchTool::new()));
        tool_registry.register(Box::new(ScheduleTool::new(schedule_manager)));
        if let Some(api_key) = brave_api_key {
            if !api_key.is_empty() {
                tool_registry.register(Box::new(WebSearchTool::new(api_key)));
            }
        }

        Self {
            router,
            agents: agents_map,
            personas,
            session_mgr,
            skills_root: workspace_root.join("skills"),
            skill_registry,
            memory,
            bus,
            runtime,
            workspace_root,
            agent_workspaces,
            file_store,
            session_writer,
            session_reader,
            search_index,
            embedding_provider,
            tool_registry,
            default_workspace_root: effective_project_root,
            react_max_steps: 4,
            react_repeat_guard: 2,
        }
    }

    fn workspace_root_for(&self, agent_id: &str) -> std::path::PathBuf {
        self.agent_workspaces
            .get(agent_id)
            .map(|ws| ws.workspace.root().to_path_buf())
            .unwrap_or_else(|| self.default_workspace_root.clone())
    }

    fn active_skill_registry(&self) -> SkillRegistry {
        SkillRegistry::load_from_dir(&self.skills_root).unwrap_or_else(|e| {
            tracing::warn!(
                "Failed to reload skills from {}: {e}",
                self.skills_root.display()
            );
            self.skill_registry.clone()
        })
    }

    fn forced_skill_names(input: &str) -> Option<Vec<String>> {
        let trimmed = input.trim();
        let rest = trimmed.strip_prefix("/skill ")?;
        let names_part = rest.split_whitespace().next()?.trim();
        if names_part.is_empty() {
            return None;
        }

        let names: Vec<String> = names_part
            .split(',')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect();

        if names.is_empty() {
            None
        } else {
            Some(names)
        }
    }

    fn merge_permissions(
        perms: impl IntoIterator<Item = corral_core::Permissions>,
    ) -> Option<corral_core::Permissions> {
        let mut list: Vec<corral_core::Permissions> = perms.into_iter().collect();
        if list.is_empty() {
            return None;
        }

        let mut merged = corral_core::Permissions::default();
        for p in list.drain(..) {
            merged.fs.read.extend(p.fs.read);
            merged.fs.write.extend(p.fs.write);
            merged.network.allow.extend(p.network.allow);
            merged.exec.extend(p.exec);
            merged.env.extend(p.env);
            merged.services.extend(p.services);
        }

        merged.fs.read.sort();
        merged.fs.read.dedup();
        merged.fs.write.sort();
        merged.fs.write.dedup();
        merged.network.allow.sort();
        merged.network.allow.dedup();
        merged.exec.sort();
        merged.exec.dedup();
        merged.env.sort();
        merged.env.dedup();

        Some(merged)
    }

    fn build_runtime_system_prompt(&self, agent_id: &str, base_prompt: String) -> String {
        let workspace_root = self.workspace_root_for(agent_id);
        format!(
            "{base_prompt}\n\nRuntime:\n- Working directory: {}",
            workspace_root.display()
        )
    }

    async fn execute_tool_for_agent(
        &self,
        agent_id: &str,
        name: &str,
        input: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<super::tool::ToolOutput> {
        match name {
            "read" => ReadFileTool::new(self.workspace_root_for(agent_id))
                .execute(input, ctx)
                .await,
            "write" => WriteFileTool::new(self.workspace_root_for(agent_id))
                .execute(input, ctx)
                .await,
            "edit" => EditFileTool::new(self.workspace_root_for(agent_id))
                .execute(input, ctx)
                .await,
            "exec" | "execute_command" => {
                ExecuteCommandTool::new(self.workspace_root_for(agent_id), 30)
                    .execute(input, ctx)
                    .await
            }
            _ => self.tool_registry.execute(name, input, ctx).await,
        }
    }

    /// Get file store for a specific agent (falls back to global)
    fn file_store_for(&self, agent_id: &str) -> &MemoryFileStore {
        self.agent_workspaces
            .get(agent_id)
            .map(|ws| &ws.file_store)
            .unwrap_or(&self.file_store)
    }

    /// Get session writer for a specific agent (falls back to global)
    fn session_writer_for(&self, agent_id: &str) -> &SessionWriter {
        self.agent_workspaces
            .get(agent_id)
            .map(|ws| &ws.session_writer)
            .unwrap_or(&self.session_writer)
    }

    /// Get session reader for a specific agent (falls back to global)
    fn session_reader_for(&self, agent_id: &str) -> &SessionReader {
        self.agent_workspaces
            .get(agent_id)
            .map(|ws| &ws.session_reader)
            .unwrap_or(&self.session_reader)
    }

    /// Get search index for a specific agent (falls back to global)
    fn search_index_for(&self, agent_id: &str) -> &SearchIndex {
        self.agent_workspaces
            .get(agent_id)
            .map(|ws| &ws.search_index)
            .unwrap_or(&self.search_index)
    }

    /// Ensure workspace directories exist for all agents
    pub async fn ensure_workspaces(&self) -> Result<()> {
        for state in self.agent_workspaces.values() {
            state.workspace.ensure_dirs().await?;
        }
        Ok(())
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
        let session_result = self
            .session_mgr
            .get_or_create(&session_key, agent_id)
            .await?;

        if session_result.expired_previous {
            self.try_fallback_summary(agent_id, &session_key, agent).await;
        }

        // Save inbound data before it's moved
        let inbound_at = inbound.at;
        let inbound_text = inbound.text.clone();

        let system_prompt = self
            .personas
            .get(agent_id)
            .map(|p| p.assembled_system_prompt())
            .unwrap_or_default();
        let active_skills = self.active_skill_registry();
        let skill_summary = active_skills.summary_prompt();
        let mut system_prompt = if skill_summary.is_empty() {
            system_prompt
        } else {
            format!("{system_prompt}\n\n{skill_summary}")
        };
        let forced_skills = Self::forced_skill_names(&inbound.text);
        let merged_permissions = if let Some(ref forced_names) = forced_skills {
            let mut missing = Vec::new();
            let selected_perms = forced_names
                .iter()
                .filter_map(|forced| {
                    if let Some(skill) = active_skills.get(forced) {
                        skill.permissions.as_ref().map(|p| p.to_corral_permissions())
                    } else {
                        missing.push(forced.clone());
                        None
                    }
                })
                .collect::<Vec<_>>();

            if forced_names.len() == 1 {
                system_prompt.push_str(&format!(
                    "\n\n## Forced Skill\nYou must follow skill '{}' for this request and prioritize its instructions over generic approaches.",
                    forced_names[0]
                ));
            } else {
                system_prompt.push_str(&format!(
                    "\n\n## Forced Skill\nYou must follow only these skills for this request: {}. Prioritize their instructions over generic approaches.",
                    forced_names.join(", ")
                ));
            }
            if !missing.is_empty() {
                system_prompt.push_str(&format!(
                    "\nMissing forced skills: {}. Tell the user these were not found.",
                    missing.join(", ")
                ));
            }

            Self::merge_permissions(selected_perms)
        } else {
            None
        };
        let system_prompt = self.build_runtime_system_prompt(agent_id, system_prompt);

        let memory_context = self
            .build_memory_context(agent_id, &session_key, &inbound.text)
            .await?;

        let history_messages = match self
            .session_reader_for(agent_id)
            .load_recent_messages(&session_key.0, 10)
            .await
        {
            Ok(msgs) => msgs,
            Err(e) => {
                tracing::warn!("Failed to load session history: {e}");
                Vec::new()
            }
        };

        let mut messages = Vec::new();
        if !memory_context.is_empty() {
            messages.push(LlmMessage::user(format!(
                "[memory context]\n{memory_context}"
            )));
            messages.push(LlmMessage::assistant("Understood, I have the context."));
        }
        for hist_msg in &history_messages {
            messages.push(LlmMessage {
                role: hist_msg.role.clone(),
                content: vec![nanocrab_provider::ContentBlock::Text {
                    text: hist_msg.content.clone(),
                }],
            });
        }
        messages.push(LlmMessage::user(
            self.runtime.preprocess_input(&inbound.text).await?,
        ));

        let allowed = agent.tool_policy.as_ref().map(|tp| tp.allow.clone());
        let (resp, _messages) = self
            .tool_use_loop(
                agent_id,
                &agent.model_policy.primary,
                &agent.model_policy.fallbacks,
                Some(system_prompt),
                messages,
                2048,
                allowed.as_deref(),
                merged_permissions,
            )
            .await?;
        let reply_text = self.runtime.postprocess_output(&resp.text).await?;

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
            .session_writer_for(agent_id)
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
            .session_writer_for(agent_id)
            .append_message(&session_key.0, "assistant", &outbound.text)
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

    /// Streaming variant of handle_inbound. Runs the tool_use_loop for
    /// intermediate tool calls, then streams the final LLM response.
    /// Publishes StreamDelta events to the bus for TUI consumption.
    pub async fn handle_inbound_stream(
        &self,
        inbound: InboundMessage,
        agent_id: &str,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send + '_>>> {
        let agent = self
            .agents
            .get(agent_id)
            .ok_or_else(|| anyhow!("agent not found: {agent_id}"))?;

        let session_key = SessionKey::from_inbound(&inbound);
        let session_result = self
            .session_mgr
            .get_or_create(&session_key, agent_id)
            .await?;

        if session_result.expired_previous {
            self.try_fallback_summary(agent_id, &session_key, agent).await;
        }

        let system_prompt = self
            .personas
            .get(agent_id)
            .map(|p| p.assembled_system_prompt())
            .unwrap_or_default();
        let active_skills = self.active_skill_registry();
        let skill_summary = active_skills.summary_prompt();
        let mut system_prompt = if skill_summary.is_empty() {
            system_prompt
        } else {
            format!("{system_prompt}\n\n{skill_summary}")
        };
        let forced_skills = Self::forced_skill_names(&inbound.text);
        let merged_permissions = if let Some(ref forced_names) = forced_skills {
            let mut missing = Vec::new();
            let selected_perms = forced_names
                .iter()
                .filter_map(|forced| {
                    if let Some(skill) = active_skills.get(forced) {
                        skill.permissions.as_ref().map(|p| p.to_corral_permissions())
                    } else {
                        missing.push(forced.clone());
                        None
                    }
                })
                .collect::<Vec<_>>();

            if forced_names.len() == 1 {
                system_prompt.push_str(&format!(
                    "\n\n## Forced Skill\nYou must follow skill '{}' for this request and prioritize its instructions over generic approaches.",
                    forced_names[0]
                ));
            } else {
                system_prompt.push_str(&format!(
                    "\n\n## Forced Skill\nYou must follow only these skills for this request: {}. Prioritize their instructions over generic approaches.",
                    forced_names.join(", ")
                ));
            }
            if !missing.is_empty() {
                system_prompt.push_str(&format!(
                    "\nMissing forced skills: {}. Tell the user these were not found.",
                    missing.join(", ")
                ));
            }

            Self::merge_permissions(selected_perms)
        } else {
            None
        };
        let system_prompt = self.build_runtime_system_prompt(agent_id, system_prompt);

        let memory_context = self
            .build_memory_context(agent_id, &session_key, &inbound.text)
            .await?;

        let history_messages = match self
            .session_reader_for(agent_id)
            .load_recent_messages(&session_key.0, 10)
            .await
        {
            Ok(msgs) => msgs,
            Err(e) => {
                tracing::warn!("Failed to load session history: {e}");
                Vec::new()
            }
        };

        let mut messages = Vec::new();
        if !memory_context.is_empty() {
            messages.push(LlmMessage::user(format!(
                "[memory context]\n{memory_context}"
            )));
            messages.push(LlmMessage::assistant("Understood, I have the context."));
        }
        for hist_msg in &history_messages {
            messages.push(LlmMessage {
                role: hist_msg.role.clone(),
                content: vec![nanocrab_provider::ContentBlock::Text {
                    text: hist_msg.content.clone(),
                }],
            });
        }
        messages.push(LlmMessage::user(
            self.runtime.preprocess_input(&inbound.text).await?,
        ));

        let allowed_stream = agent.tool_policy.as_ref().map(|tp| tp.allow.clone());
        let (_resp, final_messages) = self
            .tool_use_loop(
                agent_id,
                &agent.model_policy.primary,
                &agent.model_policy.fallbacks,
                Some(system_prompt.clone()),
                messages,
                2048,
                allowed_stream.as_deref(),
                merged_permissions,
            )
            .await?;

        let trace_id = inbound.trace_id;
        let bus = self.bus.clone();

        let stream = self
            .router
            .stream(
                &agent.model_policy.primary,
                &agent.model_policy.fallbacks,
                Some(system_prompt),
                final_messages,
                2048,
            )
            .await?;

        let mapped = tokio_stream::StreamExt::map(stream, move |chunk_result| {
            if let Ok(ref chunk) = chunk_result {
                let bus = bus.clone();
                let msg = BusMessage::StreamDelta {
                    trace_id,
                    delta: chunk.delta.clone(),
                    is_final: chunk.is_final,
                };
                tokio::spawn(async move {
                    let _ = bus.publish(msg).await;
                });
            }
            chunk_result
        });

        Ok(Box::pin(mapped))
    }

    /// Runs the tool-use loop: sends messages to the LLM, executes any
    /// requested tools, appends tool results, and repeats until the LLM
    /// produces a final (non-tool-use) response.
    ///
    /// Returns both the final LLM response **and** the accumulated messages
    /// (including all intermediate assistant/tool_result turns). Callers that
    /// need the full conversation context (e.g. `handle_inbound_stream`)
    /// should use the returned messages instead of the original input.
    async fn tool_use_loop(
        &self,
        agent_id: &str,
        primary: &str,
        fallbacks: &[String],
        system: Option<String>,
        initial_messages: Vec<LlmMessage>,
        max_tokens: u32,
        allowed_tools: Option<&[String]>,
        merged_permissions: Option<corral_core::Permissions>,
    ) -> Result<(nanocrab_provider::LlmResponse, Vec<LlmMessage>)> {
        let mut messages = initial_messages;
        let tool_defs: Vec<_> = match allowed_tools {
            Some(allow_list) => self
                .tool_registry
                .tool_defs()
                .into_iter()
                .filter(|t| allow_list.iter().any(|a| t.name.starts_with(a)))
                .collect(),
            None => self.tool_registry.tool_defs(),
        };
        let max_iterations = 10;

        for _iteration in 0..max_iterations {
            let req = LlmRequest {
                model: primary.into(),
                system: system.clone(),
                messages: messages.clone(),
                max_tokens,
                tools: tool_defs.clone(),
            };

            let resp = self.router.chat_with_tools(primary, fallbacks, req).await?;

            let tool_uses: Vec<_> = resp
                .content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::ToolUse { id, name, input } => {
                        Some((id.clone(), name.clone(), input.clone()))
                    }
                    _ => None,
                })
                .collect();

            if tool_uses.is_empty() || resp.stop_reason.as_deref() != Some("tool_use") {
                return Ok((resp, messages));
            }

            messages.push(LlmMessage {
                role: "assistant".into(),
                content: resp.content.clone(),
            });

            let mut tool_results = Vec::new();
            let recent_messages = collect_recent_messages(&messages, 20);
            let ctx = match merged_permissions.as_ref() {
                Some(perms) => ToolContext::new(corral_core::PolicyEngine::new(perms.clone())),
                None => ToolContext::default_policy(&self.workspace_root),
            }
            .with_recent_messages(recent_messages);
            for (id, name, input) in tool_uses {
                let result = match self
                    .execute_tool_for_agent(agent_id, &name, input, &ctx)
                    .await
                {
                    Ok(output) => ContentBlock::ToolResult {
                        tool_use_id: id,
                        content: output.content,
                        is_error: output.is_error,
                    },
                    Err(e) => ContentBlock::ToolResult {
                        tool_use_id: id,
                        content: format!("Tool execution error: {e}"),
                        is_error: true,
                    },
                };
                tool_results.push(result);
            }

            messages.push(LlmMessage {
                role: "user".into(),
                content: tool_results,
            });
        }

        // Loop exhausted — ask the LLM for a final answer without tools
        // so the user gets a response instead of an opaque error.
        tracing::warn!("tool_use_loop exhausted {max_iterations} iterations, requesting final answer without tools");
        let final_req = LlmRequest {
            model: primary.into(),
            system: system.clone(),
            messages: messages.clone(),
            max_tokens,
            tools: vec![],
        };
        let resp = self
            .router
            .chat_with_tools(primary, fallbacks, final_req)
            .await?;
        Ok((resp, messages))
    }

    #[allow(dead_code)]
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
            messages.push(LlmMessage::assistant(reply));
        }

        Ok(last_reply)
    }

    async fn try_fallback_summary(&self, agent_id: &str, session_key: &SessionKey, agent: &FullAgentConfig) {
        let messages = match self
            .session_reader_for(agent_id)
            .load_recent_messages(&session_key.0, 20)
            .await
        {
            Ok(msgs) if !msgs.is_empty() => msgs,
            _ => return,
        };

        let today = chrono::Utc::now().date_naive();
        if let Ok(Some(_)) = self.file_store_for(agent_id).read_daily(today).await {
            return;
        }

        let conversation = messages
            .iter()
            .map(|m| format!("{}: {}", m.role, m.content))
            .collect::<Vec<_>>()
            .join("\n");

        let system = "Summarize this conversation in 2-4 bullet points. \
            Focus on key facts, decisions, and user preferences. \
            Output Markdown bullet points only, no preamble."
            .to_string();

        let llm_messages = vec![LlmMessage::user(conversation)];

        match self
            .router
            .chat(
                &agent.model_policy.primary,
                &agent.model_policy.fallbacks,
                Some(system),
                llm_messages,
                512,
            )
            .await
        {
            Ok(resp) => {
                if let Err(e) = self.file_store_for(agent_id).append_daily(today, &resp.text).await {
                    tracing::warn!("Failed to write fallback summary: {e}");
                } else {
                    tracing::info!("Wrote fallback summary for expired session");
                }
            }
            Err(e) => {
                tracing::warn!("Failed to generate fallback summary: {e}");
            }
        }
    }

    async fn build_memory_context(&self, agent_id: &str, _session_key: &SessionKey, query: &str) -> Result<String> {
        let results = self
            .search_index_for(agent_id)
            .search(query, self.embedding_provider.as_ref(), 6, 0.25)
            .await;

        match results {
            Ok(results) if !results.is_empty() => {
                let mut context = String::from("## Relevant Memory\n\n");
                for result in &results {
                    context.push_str(&format!(
                        "### {} (score: {:.2})\n{}\n\n",
                        result.path, result.score, result.text
                    ));
                }
                Ok(context)
            }
            _ => self.file_store_for(agent_id).build_memory_context().await,
        }
    }
}

fn collect_recent_messages(messages: &[LlmMessage], limit: usize) -> Vec<ConversationMessage> {
    let mut collected = Vec::new();

    for message in messages.iter().rev() {
        let mut parts = Vec::new();
        for block in &message.content {
            if let ContentBlock::Text { text } = block {
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    parts.push(trimmed.to_string());
                }
            }
        }

        if !parts.is_empty() {
            collected.push(ConversationMessage {
                role: message.role.clone(),
                content: parts.join("\n"),
            });
            if collected.len() >= limit {
                break;
            }
        }
    }

    collected.reverse();
    collected
}
