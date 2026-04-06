use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{anyhow, Context as _, Result};
use clawhive_memory::dirty_sources::{DirtySourceStore, DIRTY_KIND_DAILY_FILE};
use clawhive_memory::file_store::MemoryFileStore;
use clawhive_memory::search_index::SearchIndex;
use clawhive_memory::{SessionReader, SessionWriter};

use crate::access_gate::AccessGate;
use crate::config_view::ConfigView;
use crate::workspace::Workspace;
use crate::workspace_manager::AgentWorkspaceState;

use super::Orchestrator;

impl Orchestrator {
    pub(super) async fn session_compaction_lock(
        &self,
        session_key: &str,
    ) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.compaction_locks.lock().await;
        locks
            .entry(session_key.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    /// Handle `/model provider/model` — validate, persist, and apply model change.
    pub(super) fn handle_model_change(
        &self,
        view: &Arc<ConfigView>,
        agent_id: &str,
        new_model: &str,
    ) -> Result<String> {
        // 1. Parse provider/model
        let (provider_id, model_name) = new_model
            .split_once('/')
            .ok_or_else(|| anyhow!("格式错误，请使用 provider/model 格式，如: openai/gpt-5.2"))?;

        if provider_id.is_empty() || model_name.is_empty() {
            anyhow::bail!("provider 和 model 不能为空，请使用格式: provider/model");
        }

        // 2. Validate provider exists in registry
        if !view.router.has_provider(provider_id) {
            let mut available = view.router.provider_ids();
            available.sort();
            let available = available.join(", ");
            anyhow::bail!("未找到 provider \"{provider_id}\"\n可用 providers: {available}");
        }

        // 3. Validate model exists in presets (only if provider has presets with models)
        if let Some(preset) = clawhive_schema::provider_presets::preset_by_id(provider_id) {
            if !preset.models.is_empty() && !preset.models.iter().any(|m| m.id == model_name) {
                let available =
                    clawhive_schema::provider_presets::provider_models_for_id(provider_id)
                        .join(", ");
                anyhow::bail!(
                    "provider \"{provider_id}\" 中未找到模型 \"{model_name}\"\n可用模型: {available}"
                );
            }
        }

        // 4. Persist to YAML
        let agent_yaml_path = self
            .workspace_root
            .join("config/agents.d")
            .join(format!("{agent_id}.yaml"));

        let yaml_content = std::fs::read_to_string(&agent_yaml_path)
            .with_context(|| format!("读取 agent 配置失败: {}", agent_yaml_path.display()))?;

        let mut doc: serde_yaml::Value =
            serde_yaml::from_str(&yaml_content).with_context(|| "解析 agent YAML 失败")?;

        doc.get_mut("model_policy")
            .and_then(|mp| mp.get_mut("primary"))
            .map(|primary| *primary = serde_yaml::Value::String(new_model.to_string()))
            .ok_or_else(|| anyhow!("agent YAML 中未找到 model_policy.primary 字段"))?;

        let updated_yaml = serde_yaml::to_string(&doc)?;
        std::fs::write(&agent_yaml_path, &updated_yaml)
            .with_context(|| format!("写入 agent 配置失败: {}", agent_yaml_path.display()))?;

        // 5. Swap in-memory config
        let mut agents = view.agents.clone();
        if let Some(agent_arc) = agents.get_mut(agent_id) {
            let mut agent = agent_arc.as_ref().clone();
            agent.model_policy.primary = new_model.to_string();
            *agent_arc = Arc::new(agent);
        }

        let new_view = ConfigView {
            generation: view.generation + 1,
            agents,
            personas: view.personas.clone(),
            routing: view.routing.clone(),
            router: view.router.clone(),
            tool_registry: view.tool_registry.clone(),
            embedding_provider: Arc::clone(&view.embedding_provider),
        };
        self.config_view.store(Arc::new(new_view));

        tracing::info!(
            agent_id = %agent_id,
            new_model = %new_model,
            "model changed via /model command"
        );

        Ok(format!("✅ 模型已切换为 **{new_model}**（已保存）"))
    }

    pub(super) fn file_store_for(&self, agent_id: &str) -> MemoryFileStore {
        self.workspaces.file_store(agent_id)
    }

    pub(super) fn workspace_state_for(&self, agent_id: &str) -> Arc<AgentWorkspaceState> {
        self.workspaces.get(agent_id)
    }

    pub(super) fn search_index_for(&self, agent_id: &str) -> SearchIndex {
        self.workspaces.search_index(agent_id)
    }

    pub(super) async fn enqueue_dirty_source(
        &self,
        agent_id: &str,
        source_kind: &str,
        source_ref: &str,
        reason: &str,
    ) {
        let dirty = DirtySourceStore::new(self.memory.db());
        if let Err(error) = dirty
            .enqueue(agent_id, source_kind, source_ref, reason)
            .await
        {
            tracing::warn!(
                agent_id = %agent_id,
                source_kind = %source_kind,
                source_ref = %source_ref,
                %error,
                "failed to enqueue dirty source"
            );
        }
    }

    pub(super) async fn drain_dirty_sources(
        &self,
        view: &ConfigView,
        agent_id: &str,
        limit: usize,
    ) {
        let workspace = self.workspace_state_for(agent_id);
        let dirty = DirtySourceStore::new(self.memory.db());
        if let Err(error) = workspace
            .search_index
            .process_dirty_sources(
                &dirty,
                agent_id,
                &workspace.file_store,
                &workspace.session_reader,
                view.embedding_provider.as_ref(),
                limit,
            )
            .await
        {
            tracing::warn!(agent_id = %agent_id, %error, "failed to index dirty sources");
        }
    }

    pub(super) async fn process_session_close_daily_dirty(
        &self,
        view: &ConfigView,
        agent_id: &str,
        session_date: chrono::NaiveDate,
    ) {
        let daily_path = format!("memory/{}.md", session_date.format("%Y-%m-%d"));
        self.enqueue_dirty_source(
            agent_id,
            DIRTY_KIND_DAILY_FILE,
            &daily_path,
            "session_close",
        )
        .await;
        self.drain_dirty_sources(view, agent_id, 8).await;
    }

    pub async fn ensure_workspaces(&self) -> Result<()> {
        self.workspaces.ensure_all().await
    }

    pub fn ensure_workspaces_for(
        &self,
        config: &crate::config::ClawhiveConfig,
        agent_ids: &[String],
    ) {
        let current = self.workspaces.load_full();
        let mut new_map: HashMap<String, Arc<AgentWorkspaceState>> = (*current).clone();
        for agent_id in agent_ids {
            if new_map.contains_key(agent_id) {
                continue;
            }
            let agent_cfg = config.agents.iter().find(|a| &a.agent_id == agent_id);
            let ws = Workspace::resolve(
                &self.workspace_root,
                agent_id,
                agent_cfg.and_then(|a| a.workspace.as_deref()),
            );
            let ws_root = ws.root().to_path_buf();
            let gate = Arc::new(AccessGate::new(ws_root.clone(), ws.access_policy_path()));
            let state = AgentWorkspaceState {
                workspace: ws,
                file_store: MemoryFileStore::new(&ws_root),
                session_writer: SessionWriter::new(&ws_root),
                session_reader: SessionReader::new(&ws_root),
                search_index: SearchIndex::new(self.memory.db(), agent_id),
                access_gate: gate,
            };
            new_map.insert(agent_id.clone(), Arc::new(state));
        }
        self.workspaces.swap_workspaces(new_map);
    }

    /// Get a reference to the hook registry for registering hooks.
    pub fn hook_registry(&self) -> &crate::hooks::HookRegistry {
        &self.hook_registry
    }
}
