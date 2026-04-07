use std::collections::HashSet;
use std::sync::Arc;

use anyhow::Result;
use chrono::Utc;
use clawhive_memory::dirty_sources::{DirtySourceStore, DIRTY_KIND_SESSION};
use clawhive_memory::embedding::EmbeddingProvider;
use clawhive_memory::file_store::MemoryFileStore;
use clawhive_memory::search_index::SearchIndex;
use clawhive_memory::{
    EpisodeStateRecord, EpisodeStatusRecord, MemoryStore, SessionReader, SessionWriter,
};
use clawhive_provider::LlmMessage;
use clawhive_schema::*;
use tokio_util::sync::CancellationToken;

use crate::config::FullAgentConfig;
use crate::config_view::ConfigView;
use crate::orchestrator::predicates::{filter_no_reply, session_reset_policy_for};
use crate::orchestrator::summary::SummaryGenerationRequest;
use crate::orchestrator::Orchestrator;
use crate::session::Session;
use crate::slash_commands;

use super::boundary::{
    build_boundary_episodes_from_state, collect_unflushed_boundary_episodes,
    collect_unflushed_boundary_turns, episode_status_ready_for_boundary_flush,
    split_boundary_flush_episode_batches, MAX_BOUNDARY_FLUSH_TURNS_PER_BATCH,
};
use super::types::BoundaryFlushSnapshot;

impl Orchestrator {
    /// Handle the flow after a /reset or /new command.
    /// This creates a fresh session and injects the post-reset prompt to guide the agent.
    pub(in crate::orchestrator) async fn handle_post_reset_flow(
        &self,
        view: &ConfigView,
        inbound: InboundMessage,
        agent_id: &str,
        agent: &FullAgentConfig,
        session_key: &SessionKey,
        post_reset_prompt: &str,
    ) -> Result<OutboundMessage> {
        // Create a fresh session
        let fresh_session = self
            .session_mgr
            .get_or_create_with_policy(session_key, agent_id, Some(session_reset_policy_for(agent)))
            .await?
            .session;

        // Build system prompt with post-reset context
        let system_prompt = view
            .persona(agent_id)
            .map(|p| p.assembled_system_prompt())
            .unwrap_or_default();
        let active_skills = self.active_skill_registry();
        let skill_summary = active_skills.summary_prompt();
        let system_prompt = if skill_summary.is_empty() {
            system_prompt
        } else {
            format!("{system_prompt}\n\n{skill_summary}")
        };
        let system_prompt =
            self.build_runtime_system_prompt(agent_id, &agent.model_policy.primary, system_prompt);

        // Build messages with post-reset prompt
        let messages = vec![LlmMessage::user(post_reset_prompt.to_string())];

        let source_info = Some((
            inbound.channel_type.clone(),
            inbound.connector_id.clone(),
            inbound.conversation_scope.clone(),
            inbound.user_scope.clone(),
        ));

        let (resp, _messages, _tool_attachments, _tool_meta) = self
            .tool_use_loop(
                view,
                agent_id,
                &fresh_session.session_key.0,
                &agent.model_policy.primary,
                &agent.model_policy.fallbacks,
                Some(system_prompt),
                messages,
                2048,
                agent
                    .tool_policy
                    .as_ref()
                    .map(|tp| tp.allow.as_slice())
                    .filter(|v| !v.is_empty()),
                None,
                agent.security.clone(),
                agent
                    .sandbox
                    .as_ref()
                    .map(|s| s.dangerous_allow_private.clone())
                    .unwrap_or_default(),
                source_info,
                false, // must_use_web_search
                false, // is_scheduled_task
                agent.model_policy.thinking_level,
                CancellationToken::new(),
            )
            .await?;

        let reply_text = self.runtime.postprocess_output(&resp.text).await?;
        let reply_text = filter_no_reply(&reply_text);

        // Record the assistant's response in the fresh session
        let workspace = self.workspace_state_for(agent_id);
        let mut session_changed = false;
        match workspace
            .session_writer
            .append_message(&fresh_session.session_id, "system", post_reset_prompt)
            .await
        {
            Err(e) => {
                tracing::warn!("Failed to write post-reset prompt to session: {e}");
            }
            _ => {
                session_changed = true;
            }
        }
        match workspace
            .session_writer
            .append_message(&fresh_session.session_id, "assistant", &reply_text)
            .await
        {
            Err(e) => {
                tracing::warn!("Failed to write assistant session entry: {e}");
            }
            _ => {
                session_changed = true;
            }
        }
        if session_changed {
            self.enqueue_dirty_source(
                agent_id,
                DIRTY_KIND_SESSION,
                &fresh_session.session_id,
                "session_reset",
            )
            .await;
            self.drain_dirty_sources(view, agent_id, 8).await;
        }

        let outbound = OutboundMessage {
            trace_id: inbound.trace_id,
            channel_type: inbound.channel_type,
            connector_id: inbound.connector_id,
            conversation_scope: inbound.conversation_scope,
            text: reply_text,
            at: chrono::Utc::now(),
            reply_to: None,
            attachments: vec![],
        };

        if !outbound.text.is_empty() {
            let preview_end = outbound.text.floor_char_boundary(200);
            tracing::info!(
                agent_id = %agent_id,
                reply_len = outbound.text.len(),
                reply_preview = &outbound.text[..preview_end],
                "agent reply"
            );
        }

        let _ = self
            .bus
            .publish(BusMessage::ReplyReady {
                outbound: outbound.clone(),
            })
            .await;

        Ok(outbound)
    }

    pub(in crate::orchestrator) async fn capture_boundary_flush_snapshot(
        &self,
        agent_id: &str,
        session: &Session,
        _agent: &FullAgentConfig,
    ) -> Option<BoundaryFlushSnapshot> {
        if session.session_key.is_scheduled_session() {
            return None;
        }
        let workspace = self.workspace_state_for(agent_id);
        let state = self
            .memory
            .get_session_memory_state(agent_id, &session.session_id)
            .await
            .ok()
            .flatten();
        let entries = workspace
            .session_reader
            .load_all_entries(&session.session_id)
            .await
            .ok()?;
        if entries.is_empty() {
            return None;
        }

        let last_flushed_turn = state
            .as_ref()
            .map(|state| state.last_flushed_turn)
            .unwrap_or(0);
        let (turns, turn_count) =
            collect_unflushed_boundary_turns(entries.clone(), last_flushed_turn)?;
        let state_episodes = state
            .as_ref()
            .map(|state| {
                let now = Utc::now();
                state
                    .open_episodes
                    .iter()
                    .filter(|episode| {
                        episode.end_turn > last_flushed_turn
                            && episode_status_ready_for_boundary_flush(episode, now)
                    })
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let episodes = if state_episodes.is_empty() {
            collect_unflushed_boundary_episodes(entries, last_flushed_turn)
                .map(|(episodes, _)| episodes)?
        } else {
            let episodes = build_boundary_episodes_from_state(&turns, &state_episodes);
            if episodes.is_empty() {
                collect_unflushed_boundary_episodes(entries, last_flushed_turn)
                    .map(|(episodes, _)| episodes)?
            } else {
                episodes
            }
        };

        let unflushed_turns = turn_count.saturating_sub(last_flushed_turn);
        if unflushed_turns > MAX_BOUNDARY_FLUSH_TURNS_PER_BATCH {
            tracing::warn!(
                %agent_id,
                session_id = %session.session_id,
                unflushed_turns,
                batch_turn_limit = MAX_BOUNDARY_FLUSH_TURNS_PER_BATCH,
                "Boundary flush has long unflushed history; splitting flush into turn-capped batches"
            );
        }

        Some(BoundaryFlushSnapshot {
            episodes,
            turn_count,
            recent_explicit_writes: state
                .map(|state| state.recent_explicit_writes)
                .unwrap_or_default(),
        })
    }

    pub(in crate::orchestrator) async fn spawn_closed_episode_flush(
        &self,
        view: &ConfigView,
        agent_id: &str,
        session: &Session,
        agent: &FullAgentConfig,
        episode: EpisodeStateRecord,
    ) {
        let Some((boundary_episode, recent_explicit_writes)) = self
            .capture_closed_episode_snapshot(agent_id, session, &episode)
            .await
        else {
            return;
        };

        let current = self
            .memory
            .get_session_memory_state(agent_id, &session.session_id)
            .await
            .unwrap_or(None);
        let Some(mut state) = current else {
            return;
        };
        if let Some(current_episode) = state
            .open_episodes
            .iter_mut()
            .find(|current_episode| current_episode.episode_id == episode.episode_id)
        {
            current_episode.status = EpisodeStatusRecord::FlushPending;
            current_episode.last_activity_at = Utc::now();
        }
        if let Err(error) = self.memory.upsert_session_memory_state(state).await {
            tracing::warn!(
                %error,
                %agent_id,
                session_id = %session.session_id,
                episode_id = %episode.episode_id,
                "Failed to persist flush-pending episode state"
            );
            return;
        }

        let router = view.router.clone();
        let file_store = self.file_store_for(agent_id);
        let memory = Arc::clone(&self.memory);
        let embedding_provider = Arc::clone(&view.embedding_provider);
        let agent_id = agent_id.to_string();
        let session = session.clone();
        let agent = agent.clone();
        tokio::spawn(async move {
            let source = format!("episode_closure:{}", episode.episode_id);
            let success =
                Orchestrator::generate_summary_from_messages_static(SummaryGenerationRequest {
                    router: &router,
                    file_store: &file_store,
                    memory: &memory,
                    embedding_provider: &embedding_provider,
                    agent_id: &agent_id,
                    session: &session,
                    agent: &agent,
                    source: &source,
                    messages: boundary_episode.messages,
                    recent_explicit_writes,
                })
                .await;

            Orchestrator::update_closed_episode_flush_state(
                &memory,
                &agent_id,
                &session,
                &episode.episode_id,
                success,
            )
            .await;
        });
    }

    pub(in crate::orchestrator) async fn schedule_session_end_flush(
        &self,
        view: &ConfigView,
        agent_id: &str,
        session: &Session,
        agent: &FullAgentConfig,
    ) {
        Self::close_open_episodes_for_session_end(&self.memory, agent_id, session).await;

        let current = self
            .memory
            .get_session_memory_state(agent_id, &session.session_id)
            .await
            .unwrap_or(None);
        let Some(state) = current else {
            return;
        };

        let mut session_end_episodes = state
            .open_episodes
            .into_iter()
            .filter(|episode| {
                episode.status != EpisodeStatusRecord::Flushed
                    && episode.status != EpisodeStatusRecord::FlushPending
            })
            .collect::<Vec<_>>();
        session_end_episodes.sort_by_key(|episode| (episode.start_turn, episode.end_turn));

        for episode in session_end_episodes {
            self.spawn_closed_episode_flush(view, agent_id, session, agent, episode)
                .await;
        }
    }

    pub(in crate::orchestrator) async fn finalize_stale_boundary_flush(
        memory: &Arc<MemoryStore>,
        agent_id: &str,
        session: &Session,
        file_store: &MemoryFileStore,
        embedding_provider: &Arc<dyn EmbeddingProvider>,
    ) {
        let session_writer = SessionWriter::new(file_store.workspace_dir());
        if let Err(error) = session_writer.archive_session(&session.session_id).await {
            tracing::warn!(
                %error,
                %agent_id,
                session_key = %session.session_key.0,
                session_id = %session.session_id,
                "Failed to archive stale session transcript after boundary flush"
            );
            return;
        }

        let _ = memory
            .delete_session_memory_state(agent_id, &session.session_id)
            .await;

        let dirty = DirtySourceStore::new(memory.db());
        if let Err(error) = dirty
            .enqueue(
                agent_id,
                DIRTY_KIND_SESSION,
                &session.session_id,
                "session_archived_after_reset",
            )
            .await
        {
            tracing::warn!(
                %error,
                %agent_id,
                session_id = %session.session_id,
                "Failed to enqueue archived stale session for reindex"
            );
            return;
        }

        let session_reader = SessionReader::new(file_store.workspace_dir());
        let search_index = SearchIndex::new(memory.db(), agent_id);
        if let Err(error) = search_index
            .process_dirty_sources(
                &dirty,
                agent_id,
                file_store,
                &session_reader,
                embedding_provider.as_ref(),
                8,
            )
            .await
        {
            tracing::warn!(
                %error,
                %agent_id,
                session_id = %session.session_id,
                "Failed to drain archived stale session dirty source"
            );
        }
    }

    pub(in crate::orchestrator) async fn recover_pending_boundary_flushes_for_session_key(
        &self,
        view: Arc<ConfigView>,
        agent_id: &str,
        session_key: &SessionKey,
        agent: &FullAgentConfig,
    ) {
        let pending = match self
            .memory
            .list_pending_session_memory_states_for_session_key(agent_id, &session_key.0, 8)
            .await
        {
            Ok(states) => states,
            Err(error) => {
                tracing::warn!(
                    %error,
                    %agent_id,
                    session_key = %session_key.0,
                    "Failed to load pending boundary flush state"
                );
                return;
            }
        };
        if pending.is_empty() {
            return;
        }

        let workspace = self.workspace_state_for(agent_id);
        for state in pending {
            if !workspace
                .session_reader
                .session_exists(&state.session_id)
                .await
            {
                tracing::warn!(
                    %agent_id,
                    session_key = %state.session_key,
                    session_id = %state.session_id,
                    "Pending boundary flush transcript is missing; keeping state for manual repair"
                );
                continue;
            }

            let mut in_flight = self.pending_boundary_recoveries.lock().await;
            if !in_flight.insert(state.session_id.clone()) {
                continue;
            }
            drop(in_flight);

            let recovery_session = Session {
                session_key: SessionKey(state.session_key.clone()),
                session_id: state.session_id.clone(),
                agent_id: agent_id.to_string(),
                created_at: Utc::now(),
                last_active: Utc::now(),
                ttl_seconds: 0,
                interaction_count: 0,
            };

            tracing::info!(
                %agent_id,
                session_key = %recovery_session.session_key.0,
                session_id = %recovery_session.session_id,
                "Recovering pending boundary flush after restart"
            );

            self.schedule_stale_boundary_flush_with_guard(
                view.clone(),
                agent_id,
                &recovery_session,
                agent,
                Some(Arc::clone(&self.pending_boundary_recoveries)),
            )
            .await;
        }
    }

    pub(in crate::orchestrator) async fn schedule_stale_boundary_flush(
        &self,
        view: Arc<ConfigView>,
        agent_id: &str,
        session: &Session,
        agent: &FullAgentConfig,
    ) {
        self.schedule_stale_boundary_flush_with_guard(view, agent_id, session, agent, None)
            .await;
    }

    pub(in crate::orchestrator) async fn schedule_stale_boundary_flush_with_guard(
        &self,
        view: Arc<ConfigView>,
        agent_id: &str,
        session: &Session,
        agent: &FullAgentConfig,
        recovery_guard: Option<Arc<tokio::sync::Mutex<HashSet<String>>>>,
    ) {
        self.schedule_session_end_flush(&view, agent_id, session, agent)
            .await;
        let Some(snapshot) = self
            .capture_boundary_flush_snapshot(agent_id, session, agent)
            .await
        else {
            self.update_boundary_flush_state(agent_id, session, None, true)
                .await;
            if let Some(guard) = recovery_guard {
                let mut in_flight = guard.lock().await;
                in_flight.remove(&session.session_id);
            }
            return;
        };

        Self::persist_boundary_flush_state(&self.memory, agent_id, session, None, false).await;

        let agent_id = agent_id.to_string();
        let session = session.clone();
        let agent = agent.clone();
        let router = view.router.clone();
        let embedding_provider = Arc::clone(&view.embedding_provider);
        let file_store = self.file_store_for(&agent_id);
        let memory = Arc::clone(&self.memory);
        let recovery_session_id = session.session_id.clone();
        tokio::spawn(async move {
            let batches = split_boundary_flush_episode_batches(
                &snapshot.episodes,
                MAX_BOUNDARY_FLUSH_TURNS_PER_BATCH,
            );
            let mut success = true;
            let mut episode_index = 0_usize;
            for batch in batches {
                for episode in batch {
                    episode_index += 1;
                    let episode_source = format!("fallback_summary:episode:{episode_index}");
                    let episode_success = Orchestrator::generate_summary_from_messages_static(
                        SummaryGenerationRequest {
                            router: &router,
                            file_store: &file_store,
                            memory: &memory,
                            embedding_provider: &embedding_provider,
                            agent_id: &agent_id,
                            session: &session,
                            agent: &agent,
                            source: &episode_source,
                            messages: episode.messages,
                            recent_explicit_writes: snapshot.recent_explicit_writes.clone(),
                        },
                    )
                    .await;
                    success &= episode_success;
                }
            }
            Orchestrator::persist_boundary_flush_state(
                &memory,
                &agent_id,
                &session,
                Some(snapshot.turn_count),
                success,
            )
            .await;

            if success {
                Orchestrator::finalize_stale_boundary_flush(
                    &memory,
                    &agent_id,
                    &session,
                    &file_store,
                    &embedding_provider,
                )
                .await;
            } else {
                tracing::warn!(
                    %agent_id,
                    session_key = %session.session_key.0,
                    session_id = %session.session_id,
                    "Asynchronous boundary flush failed for stale session; keeping transcript in place for retry"
                );
            }

            if let Some(guard) = recovery_guard {
                let mut in_flight = guard.lock().await;
                in_flight.remove(&recovery_session_id);
            }
        });
    }

    pub(in crate::orchestrator) async fn run_boundary_flush_snapshot(
        &self,
        view: &ConfigView,
        agent_id: &str,
        session: &Session,
        agent: &FullAgentConfig,
        source: &str,
        snapshot: BoundaryFlushSnapshot,
    ) -> bool {
        let mut success = true;
        let batches = split_boundary_flush_episode_batches(
            &snapshot.episodes,
            MAX_BOUNDARY_FLUSH_TURNS_PER_BATCH,
        );
        let mut episode_index = 0_usize;
        for batch in batches {
            let mut handles = Vec::with_capacity(batch.len());
            for episode in batch {
                episode_index += 1;
                let episode_source = format!("{source}:episode:{episode_index}");
                let router = view.router.clone();
                let file_store = self.file_store_for(agent_id);
                let memory = Arc::clone(&self.memory);
                let embedding_provider = Arc::clone(&view.embedding_provider);
                let agent_id_owned = agent_id.to_string();
                let session_clone = session.clone();
                let agent_clone = agent.clone();
                let recent_writes = snapshot.recent_explicit_writes.clone();
                handles.push(tokio::spawn(async move {
                    Self::generate_summary_from_messages_static(SummaryGenerationRequest {
                        router: &router,
                        file_store: &file_store,
                        memory: &memory,
                        embedding_provider: &embedding_provider,
                        agent_id: &agent_id_owned,
                        session: &session_clone,
                        agent: &agent_clone,
                        source: &episode_source,
                        messages: episode.messages,
                        recent_explicit_writes: recent_writes,
                    })
                    .await
                }));
            }
            for handle in handles {
                match handle.await {
                    Ok(result) => success &= result,
                    Err(error) => {
                        tracing::warn!(%error, "boundary flush episode task panicked");
                        success = false;
                    }
                }
            }
        }
        self.update_boundary_flush_state(agent_id, session, Some(snapshot.turn_count), success)
            .await;
        success
    }

    pub(in crate::orchestrator) async fn handle_explicit_session_reset(
        &self,
        view: &ConfigView,
        inbound: InboundMessage,
        agent_id: &str,
        agent: &FullAgentConfig,
        session_key: &SessionKey,
        model_hint: Option<&str>,
    ) -> Result<OutboundMessage> {
        let previous_session = self.session_mgr.get(session_key).await?;
        if let Some(previous_session) = previous_session.as_ref() {
            self.schedule_session_end_flush(view, agent_id, previous_session, agent)
                .await;
            if let Some(snapshot) = self
                .capture_boundary_flush_snapshot(agent_id, previous_session, agent)
                .await
            {
                let _ = self
                    .run_boundary_flush_snapshot(
                        view,
                        agent_id,
                        previous_session,
                        agent,
                        "explicit_reset",
                        snapshot,
                    )
                    .await;
            }
        }

        let _ = self.session_mgr.reset(session_key).await;
        if let Some(previous_session) = previous_session.as_ref() {
            let workspace = self.workspace_state_for(agent_id);
            let _ = workspace
                .session_writer
                .clear_session(&previous_session.session_id)
                .await;
            let _ = self
                .memory
                .delete_session_memory_state(agent_id, &previous_session.session_id)
                .await;
        }

        let post_reset_prompt = slash_commands::build_post_reset_prompt(agent_id);
        if let Some(hint) = model_hint {
            tracing::info!("Session reset with model hint: {hint}");
        }

        self.handle_post_reset_flow(
            view,
            inbound,
            agent_id,
            agent,
            session_key,
            &post_reset_prompt,
        )
        .await
    }
}
