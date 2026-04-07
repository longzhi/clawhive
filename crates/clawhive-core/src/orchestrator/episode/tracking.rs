use std::sync::Arc;

use chrono::Utc;
use clawhive_memory::{
    EpisodeStateRecord, EpisodeStatusRecord, MemoryStore, RecentExplicitMemoryWrite,
    SessionMemoryStateRecord,
};

use crate::orchestrator::Orchestrator;
use crate::session::Session;

use super::boundary::{
    boundary_flush_topic_tokens_from_text, build_episode_topic_sketch,
    collect_boundary_episode_for_range, decide_episode_turn, infer_episode_task_state,
};
use super::types::{BoundaryFlushEpisode, EpisodeBoundaryDecision, EpisodeTurnInput};

impl Orchestrator {
    pub(in crate::orchestrator) async fn record_session_turn_episode(
        &self,
        agent_id: &str,
        session: &Session,
        turn: EpisodeTurnInput<'_>,
    ) -> Option<EpisodeStateRecord> {
        let current = match self
            .memory
            .get_session_memory_state(agent_id, &session.session_id)
            .await
        {
            Ok(state) => state,
            Err(error) => {
                tracing::warn!(
                    %error,
                    %agent_id,
                    session_id = %session.session_id,
                    "Failed to load session memory state for episode tracking"
                );
                return None;
            }
        };

        let mut state = current.unwrap_or(SessionMemoryStateRecord {
            agent_id: agent_id.to_string(),
            session_id: session.session_id.clone(),
            session_key: session.session_key.0.clone(),
            last_flushed_turn: 0,
            last_boundary_flush_at: None,
            pending_flush: false,
            flush_phase: "idle".to_string(),
            flush_phase_updated_at: None,
            flush_summary_cache: None,
            recent_explicit_writes: Vec::new(),
            open_episodes: Vec::new(),
        });

        let sketch = build_episode_topic_sketch(turn.user_text);
        let current_tokens = boundary_flush_topic_tokens_from_text(&sketch);
        let now = Utc::now();
        let mut closed_episode = None;

        if let Some(last) = state
            .open_episodes
            .iter_mut()
            .rev()
            .find(|episode| episode.status == EpisodeStatusRecord::Open)
        {
            let last_tokens = boundary_flush_topic_tokens_from_text(&last.topic_sketch);
            let decision = decide_episode_turn(
                &last_tokens,
                &current_tokens,
                turn.assistant_text,
                turn.successful_tool_calls,
                turn.final_stop_reason,
                last.task_state.clone(),
                last.end_turn.saturating_sub(last.start_turn) + 1,
            );
            match decision.boundary {
                EpisodeBoundaryDecision::ContinueCurrent => {
                    last.end_turn = turn.turn_index;
                    if !sketch.is_empty() {
                        last.topic_sketch = sketch;
                    }
                    last.task_state = decision.task_state.clone();
                    last.last_activity_at = now;
                }
                EpisodeBoundaryDecision::CloseCurrentAndOpenNext => {
                    last.status = EpisodeStatusRecord::Closed;
                    last.last_activity_at = now;
                    closed_episode = Some(last.clone());
                    state.open_episodes.push(EpisodeStateRecord {
                        episode_id: format!("{}:{}", session.session_id, turn.turn_index),
                        start_turn: turn.turn_index,
                        end_turn: turn.turn_index,
                        status: EpisodeStatusRecord::Open,
                        task_state: decision.task_state.clone(),
                        topic_sketch: sketch,
                        last_activity_at: now,
                    });
                }
            }
        } else {
            let task_state = infer_episode_task_state(
                turn.assistant_text,
                turn.successful_tool_calls,
                turn.final_stop_reason,
            );
            state.open_episodes.push(EpisodeStateRecord {
                episode_id: format!("{}:{}", session.session_id, turn.turn_index),
                start_turn: turn.turn_index,
                end_turn: turn.turn_index,
                status: EpisodeStatusRecord::Open,
                task_state,
                topic_sketch: sketch,
                last_activity_at: now,
            });
        }

        if let Err(error) = self.memory.upsert_session_memory_state(state).await {
            tracing::warn!(
                %error,
                %agent_id,
                session_id = %session.session_id,
                assistant_len = turn.assistant_text.len(),
                "Failed to persist open episode state"
            );
        }

        closed_episode
    }

    pub(in crate::orchestrator) async fn capture_closed_episode_snapshot(
        &self,
        agent_id: &str,
        session: &Session,
        episode: &EpisodeStateRecord,
    ) -> Option<(BoundaryFlushEpisode, Vec<RecentExplicitMemoryWrite>)> {
        let workspace = self.workspace_state_for(agent_id);
        let entries = workspace
            .session_reader
            .load_all_entries(&session.session_id)
            .await
            .ok()?;
        let state = self
            .memory
            .get_session_memory_state(agent_id, &session.session_id)
            .await
            .ok()
            .flatten();
        let boundary_episode =
            collect_boundary_episode_for_range(entries, episode.start_turn, episode.end_turn)?;
        Some((
            boundary_episode,
            state
                .map(|state| state.recent_explicit_writes)
                .unwrap_or_default(),
        ))
    }

    pub(in crate::orchestrator) async fn update_boundary_flush_state(
        &self,
        agent_id: &str,
        session: &Session,
        turn_count: Option<u64>,
        success: bool,
    ) {
        Self::persist_boundary_flush_state(&self.memory, agent_id, session, turn_count, success)
            .await;
    }

    pub(in crate::orchestrator) async fn persist_boundary_flush_state(
        memory: &Arc<MemoryStore>,
        agent_id: &str,
        session: &Session,
        turn_count: Option<u64>,
        success: bool,
    ) {
        let current = memory
            .get_session_memory_state(agent_id, &session.session_id)
            .await
            .unwrap_or(None);
        let mut state = current.unwrap_or(SessionMemoryStateRecord {
            agent_id: agent_id.to_string(),
            session_id: session.session_id.clone(),
            session_key: session.session_key.0.clone(),
            last_flushed_turn: 0,
            last_boundary_flush_at: None,
            pending_flush: false,
            flush_phase: "idle".to_string(),
            flush_phase_updated_at: None,
            flush_summary_cache: None,
            recent_explicit_writes: Vec::new(),
            open_episodes: Vec::new(),
        });

        if success {
            if let Some(turn_count) = turn_count {
                state.last_flushed_turn = turn_count;
                state
                    .recent_explicit_writes
                    .retain(|marker| marker.turn_index > turn_count);
                state
                    .open_episodes
                    .retain(|episode| episode.end_turn > turn_count);
            }
            state.last_boundary_flush_at = Some(Utc::now());
            state.pending_flush = false;
        } else {
            state.pending_flush = true;
        }

        if let Err(error) = memory.upsert_session_memory_state(state).await {
            tracing::warn!(
                %error,
                %agent_id,
                session_key = %session.session_key.0,
                session_id = %session.session_id,
                "Failed to persist session memory state"
            );
        }
    }

    pub(in crate::orchestrator) async fn update_closed_episode_flush_state(
        memory: &Arc<MemoryStore>,
        agent_id: &str,
        session: &Session,
        episode_id: &str,
        success: bool,
    ) {
        let current = memory
            .get_session_memory_state(agent_id, &session.session_id)
            .await
            .unwrap_or(None);
        let Some(mut state) = current else {
            return;
        };

        if let Some(episode) = state
            .open_episodes
            .iter_mut()
            .find(|episode| episode.episode_id == episode_id)
        {
            if success {
                episode.status = EpisodeStatusRecord::Flushed;
            } else if episode.status == EpisodeStatusRecord::FlushPending {
                episode.status = EpisodeStatusRecord::Closed;
                episode.last_activity_at = Utc::now();
            }
        }

        if success {
            let mut checkpoint = state.last_flushed_turn;
            loop {
                let next = state
                    .open_episodes
                    .iter()
                    .filter(|episode| {
                        episode.status == EpisodeStatusRecord::Flushed
                            && episode.start_turn == checkpoint.saturating_add(1)
                    })
                    .min_by_key(|episode| episode.start_turn)
                    .cloned();

                let Some(next) = next else {
                    break;
                };
                checkpoint = checkpoint.max(next.end_turn);
            }

            if checkpoint > state.last_flushed_turn {
                state.last_flushed_turn = checkpoint;
                state
                    .recent_explicit_writes
                    .retain(|marker| marker.turn_index > checkpoint);
                state.open_episodes.retain(|episode| {
                    !(episode.status == EpisodeStatusRecord::Flushed
                        && episode.end_turn <= checkpoint)
                });
            }
            state.last_boundary_flush_at = Some(Utc::now());
        }

        if let Err(error) = memory.upsert_session_memory_state(state).await {
            tracing::warn!(
                %error,
                %agent_id,
                session_id = %session.session_id,
                episode_id,
                "Failed to persist closed episode flush state"
            );
        }
    }

    pub(in crate::orchestrator) async fn close_open_episodes_for_session_end(
        memory: &Arc<MemoryStore>,
        agent_id: &str,
        session: &Session,
    ) {
        let current = memory
            .get_session_memory_state(agent_id, &session.session_id)
            .await
            .unwrap_or(None);
        let Some(mut state) = current else {
            return;
        };

        let mut changed = false;
        let now = Utc::now();
        for episode in &mut state.open_episodes {
            if episode.status == EpisodeStatusRecord::Open {
                episode.status = EpisodeStatusRecord::Closed;
                episode.last_activity_at = now;
                changed = true;
            }
        }

        if changed {
            if let Err(error) = memory.upsert_session_memory_state(state).await {
                tracing::warn!(
                    %error,
                    %agent_id,
                    session_key = %session.session_key.0,
                    session_id = %session.session_id,
                    "Failed to close open episodes for session-end boundary flush"
                );
            }
        }
    }
}
