use std::collections::HashSet;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use chrono::Utc;
use clawhive_memory::dirty_sources::{DirtySourceStore, DIRTY_KIND_SESSION};
use clawhive_memory::embedding::EmbeddingProvider;
use clawhive_memory::file_store::MemoryFileStore;
use clawhive_memory::search_index::SearchIndex;
use clawhive_memory::{
    EpisodeStateRecord, EpisodeStatusRecord, EpisodeTaskStateRecord, MemoryStore,
    RecentExplicitMemoryWrite, SessionEntry, SessionMemoryStateRecord, SessionMessage,
    SessionReader, SessionWriter,
};
use clawhive_provider::LlmMessage;
use clawhive_schema::*;
use tokio_util::sync::CancellationToken;

use crate::config::FullAgentConfig;
use crate::config_view::ConfigView;
use crate::router::LlmRouter;
use crate::session::Session;
use crate::slash_commands;

use super::predicates::{filter_no_reply, session_reset_policy_for};
use super::{detect_empty_promise_structural, EmptyPromiseVerdict, Orchestrator};

pub(super) const EPISODE_FLUSH_PENDING_GRACE_SECS: i64 = 30;
pub(super) const MAX_OPEN_EPISODE_TURNS: u64 = 4;
pub(super) const MAX_BOUNDARY_FLUSH_TURNS_PER_BATCH: u64 = 50;

#[derive(Debug, Clone)]
pub(super) struct BoundaryFlushEpisode {
    pub(super) start_turn: u64,
    pub(super) end_turn: u64,
    pub(super) messages: Vec<SessionMessage>,
}

#[derive(Debug, Clone)]
pub(super) struct BoundaryFlushSnapshot {
    pub(super) episodes: Vec<BoundaryFlushEpisode>,
    pub(super) turn_count: u64,
    pub(super) recent_explicit_writes: Vec<RecentExplicitMemoryWrite>,
}

#[derive(Debug, Clone, Default)]
pub(super) struct ToolLoopMeta {
    pub(super) successful_tool_calls: usize,
    pub(super) final_stop_reason: Option<String>,
    pub(super) cancelled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EpisodeBoundaryDecision {
    ContinueCurrent,
    CloseCurrentAndOpenNext,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct EpisodeTurnDecision {
    pub(super) task_state: EpisodeTaskStateRecord,
    pub(super) boundary: EpisodeBoundaryDecision,
}

pub(super) struct EpisodeTurnInput<'a> {
    pub(super) turn_index: u64,
    pub(super) user_text: &'a str,
    pub(super) assistant_text: &'a str,
    pub(super) successful_tool_calls: usize,
    pub(super) final_stop_reason: Option<&'a str>,
}

pub(super) struct SummaryGenerationRequest<'a> {
    pub(super) router: &'a LlmRouter,
    pub(super) file_store: &'a clawhive_memory::file_store::MemoryFileStore,
    pub(super) memory: &'a Arc<MemoryStore>,
    pub(super) embedding_provider: &'a Arc<dyn EmbeddingProvider>,
    pub(super) agent_id: &'a str,
    pub(super) session: &'a Session,
    pub(super) agent: &'a FullAgentConfig,
    pub(super) source: &'a str,
    pub(super) messages: Vec<SessionMessage>,
    pub(super) recent_explicit_writes: Vec<RecentExplicitMemoryWrite>,
}

pub(super) fn normalized_duplicate_key(
    candidate: &crate::memory_summary::SummaryCandidate,
) -> Option<String> {
    candidate
        .duplicate_key
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

pub(super) fn normalized_candidate_fact_type(
    candidate: &crate::memory_summary::SummaryCandidate,
) -> &'static str {
    match candidate.fact_type.as_deref().map(str::trim) {
        Some("preference") => "preference",
        Some("decision") => "decision",
        Some("event") => "event",
        Some("person") => "person",
        Some("rule") => "rule",
        Some("procedure") => "procedure",
        _ => "decision",
    }
}

pub(super) fn fact_token_overlap_ratio(a: &str, b: &str) -> f64 {
    let tokens_a = crate::consolidation::normalized_word_set(a);
    let tokens_b = crate::consolidation::normalized_word_set(b);
    crate::consolidation::jaccard_similarity(&tokens_a, &tokens_b)
}

pub(crate) fn contains_correction_phrase(content: &str) -> bool {
    const PHRASES_CN: &[&str] = &[
        "不再",
        "改为",
        "已切换到",
        "改成",
        "换成",
        "已放弃",
        "不用了",
    ];
    const PHRASES_EN: &[&str] = &[
        "no longer",
        "switched to",
        "changed to",
        "moved to",
        "instead of",
        "replaced with",
        "stopped using",
        "quit using",
    ];

    let lower = content.to_lowercase();
    PHRASES_CN.iter().any(|phrase| content.contains(phrase))
        || PHRASES_EN.iter().any(|phrase| lower.contains(phrase))
}

pub(super) fn boundary_flush_conflict_passes_two_step(
    new_content: &str,
    new_fact_type: &str,
    existing: &clawhive_memory::fact_store::Fact,
    embedding_similarity: Option<f64>,
) -> bool {
    let Some(similarity) = embedding_similarity else {
        return false;
    };
    if similarity <= 0.85 {
        return false;
    }
    if existing.fact_type != new_fact_type {
        return false;
    }

    if contains_correction_phrase(new_content) {
        return true;
    }

    fact_token_overlap_ratio(new_content, &existing.content) > 0.6
}

pub(super) fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    let len = a.len().min(b.len());
    if len == 0 {
        return 0.0;
    }

    let mut dot = 0.0f32;
    let mut norm_a = 0.0f32;
    let mut norm_b = 0.0f32;
    for idx in 0..len {
        dot += a[idx] * b[idx];
        norm_a += a[idx] * a[idx];
        norm_b += b[idx] * b[idx];
    }
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }

    dot / (norm_a.sqrt() * norm_b.sqrt())
}

pub(crate) async fn find_boundary_flush_conflict(
    embedding_provider: &Arc<dyn EmbeddingProvider>,
    new_content: &str,
    new_fact_type: &str,
    active_facts: &[clawhive_memory::fact_store::Fact],
) -> Result<Option<clawhive_memory::fact_store::Fact>> {
    if active_facts.is_empty() {
        return Ok(None);
    }

    let mut texts = Vec::with_capacity(active_facts.len() + 1);
    texts.push(new_content.to_string());
    texts.extend(active_facts.iter().map(|fact| fact.content.clone()));

    let embeddings = tokio::time::timeout(
        std::time::Duration::from_millis(200),
        embedding_provider.embed(&texts),
    )
    .await
    .map_err(|_| anyhow!("boundary flush conflict embedding timed out"))??
    .embeddings;
    if embeddings.len() != texts.len() {
        return Ok(None);
    }

    let new_embedding = &embeddings[0];
    let conflict = active_facts
        .iter()
        .zip(embeddings.iter().skip(1))
        .find(|(existing, embedding)| {
            let similarity = f64::from(cosine_similarity(new_embedding, embedding));
            boundary_flush_conflict_passes_two_step(
                new_content,
                new_fact_type,
                existing,
                Some(similarity),
            )
        })
        .map(|(fact, _)| fact.clone());

    Ok(conflict)
}

pub(super) fn boundary_flush_topic_tokens(
    messages: &[SessionMessage],
) -> std::collections::HashSet<String> {
    messages
        .iter()
        .filter(|message| message.role == "user")
        .flat_map(|message| {
            message
                .content
                .split(|ch: char| !ch.is_alphanumeric())
                .map(str::trim)
                .filter(|token| token.len() >= 3)
                .map(|token| token.to_ascii_lowercase())
                .collect::<Vec<_>>()
        })
        .collect()
}

pub(super) fn boundary_flush_topic_tokens_from_text(
    text: &str,
) -> std::collections::HashSet<String> {
    text.split(|ch: char| !ch.is_alphanumeric())
        .map(str::trim)
        .filter(|token| token.len() >= 3)
        .map(|token| token.to_ascii_lowercase())
        .collect()
}

pub(super) fn build_episode_topic_sketch(text: &str) -> String {
    let mut tokens = boundary_flush_topic_tokens_from_text(text)
        .into_iter()
        .collect::<Vec<_>>();
    tokens.sort();
    tokens.truncate(8);

    if tokens.is_empty() {
        text.trim()
            .chars()
            .take(96)
            .collect::<String>()
            .trim()
            .to_string()
    } else {
        tokens.join(" ")
    }
}

pub(super) fn boundary_flush_topics_are_related(
    current: &std::collections::HashSet<String>,
    next: &std::collections::HashSet<String>,
) -> bool {
    if current.is_empty() || next.is_empty() {
        return false;
    }

    current.intersection(next).count() >= 2
}

pub(super) fn infer_episode_task_state(
    assistant_text: &str,
    successful_tool_calls: usize,
    final_stop_reason: Option<&str>,
) -> EpisodeTaskStateRecord {
    if final_stop_reason == Some("length") {
        return EpisodeTaskStateRecord::Executing;
    }

    match detect_empty_promise_structural(0, 0, assistant_text) {
        EmptyPromiseVerdict::Structural => EpisodeTaskStateRecord::Executing,
        EmptyPromiseVerdict::Inconclusive => {
            if successful_tool_calls > 0 {
                EpisodeTaskStateRecord::Delivered
            } else {
                EpisodeTaskStateRecord::Exploring
            }
        }
        EmptyPromiseVerdict::No => EpisodeTaskStateRecord::Delivered,
    }
}

pub(super) fn decide_episode_turn(
    current_tokens: &std::collections::HashSet<String>,
    next_tokens: &std::collections::HashSet<String>,
    assistant_text: &str,
    successful_tool_calls: usize,
    final_stop_reason: Option<&str>,
    current_task_state: EpisodeTaskStateRecord,
    current_turn_count: u64,
) -> EpisodeTurnDecision {
    let task_state =
        infer_episode_task_state(assistant_text, successful_tool_calls, final_stop_reason);
    let boundary = if current_turn_count >= MAX_OPEN_EPISODE_TURNS {
        EpisodeBoundaryDecision::CloseCurrentAndOpenNext
    } else if boundary_flush_topics_are_related(current_tokens, next_tokens) {
        EpisodeBoundaryDecision::ContinueCurrent
    } else {
        match current_task_state {
            EpisodeTaskStateRecord::Delivered
            | EpisodeTaskStateRecord::Executing
            | EpisodeTaskStateRecord::Exploring => EpisodeBoundaryDecision::CloseCurrentAndOpenNext,
        }
    };

    EpisodeTurnDecision {
        task_state,
        boundary,
    }
}

pub(super) fn episode_status_ready_for_boundary_flush(
    episode: &EpisodeStateRecord,
    now: chrono::DateTime<Utc>,
) -> bool {
    match episode.status {
        EpisodeStatusRecord::Open | EpisodeStatusRecord::Closed => true,
        EpisodeStatusRecord::Flushed => false,
        EpisodeStatusRecord::FlushPending => {
            now.signed_duration_since(episode.last_activity_at)
                .num_seconds()
                >= EPISODE_FLUSH_PENDING_GRACE_SECS
        }
    }
}

pub(super) fn collect_unflushed_boundary_turns(
    entries: Vec<SessionEntry>,
    last_flushed_turn: u64,
) -> Option<(Vec<BoundaryFlushEpisode>, u64)> {
    let mut turn_count = 0_u64;
    let mut include_current_turn = false;
    let mut turns = Vec::new();
    let mut current_turn_messages = Vec::new();

    for entry in entries {
        let SessionEntry::Message {
            message, timestamp, ..
        } = entry
        else {
            continue;
        };

        if message.role == "user" {
            if include_current_turn && !current_turn_messages.is_empty() {
                turns.push(BoundaryFlushEpisode {
                    start_turn: turn_count,
                    end_turn: turn_count,
                    messages: std::mem::take(&mut current_turn_messages),
                });
            }
            turn_count = turn_count.saturating_add(1);
            include_current_turn = turn_count > last_flushed_turn;
        }

        if include_current_turn {
            current_turn_messages.push(SessionMessage {
                timestamp: Some(timestamp),
                ..message
            });
        }
    }

    if include_current_turn && !current_turn_messages.is_empty() {
        turns.push(BoundaryFlushEpisode {
            start_turn: turn_count,
            end_turn: turn_count,
            messages: current_turn_messages,
        });
    }

    if turns.is_empty() {
        return None;
    }

    Some((turns, turn_count))
}

pub(super) fn collect_unflushed_boundary_episodes(
    entries: Vec<SessionEntry>,
    last_flushed_turn: u64,
) -> Option<(Vec<BoundaryFlushEpisode>, u64)> {
    let (turns, turn_count) = collect_unflushed_boundary_turns(entries, last_flushed_turn)?;

    const MAX_EPISODE_TURNS: usize = 3;
    const MAX_EPISODE_CHARS: usize = 1600;

    let mut episodes = Vec::new();
    let mut current = BoundaryFlushEpisode {
        start_turn: turns[0].start_turn,
        end_turn: turns[0].end_turn,
        messages: turns[0].messages.clone(),
    };
    let mut current_tokens = boundary_flush_topic_tokens(&current.messages);

    for next in turns.into_iter().skip(1) {
        let next_tokens = boundary_flush_topic_tokens(&next.messages);
        let current_turns = current.end_turn.saturating_sub(current.start_turn) + 1;
        let current_chars = current
            .messages
            .iter()
            .map(|message| message.content.len())
            .sum::<usize>();
        let next_chars = next
            .messages
            .iter()
            .map(|message| message.content.len())
            .sum::<usize>();
        let can_merge = current.end_turn + 1 == next.start_turn
            && current_turns < MAX_EPISODE_TURNS as u64
            && current_chars + next_chars <= MAX_EPISODE_CHARS
            && boundary_flush_topics_are_related(&current_tokens, &next_tokens);

        if can_merge {
            current.end_turn = next.end_turn;
            current.messages.extend(next.messages);
            current_tokens.extend(next_tokens);
        } else {
            episodes.push(current);
            current = BoundaryFlushEpisode {
                start_turn: next.start_turn,
                end_turn: next.end_turn,
                messages: next.messages,
            };
            current_tokens = next_tokens;
        }
    }

    episodes.push(current);

    Some((episodes, turn_count))
}

pub(super) fn build_boundary_episodes_from_state(
    turns: &[BoundaryFlushEpisode],
    open_episodes: &[EpisodeStateRecord],
) -> Vec<BoundaryFlushEpisode> {
    let mut episodes = Vec::new();
    let now = Utc::now();

    let mut ranges = open_episodes
        .iter()
        .filter(|episode| episode_status_ready_for_boundary_flush(episode, now))
        .cloned()
        .collect::<Vec<_>>();
    ranges.sort_by_key(|episode| (episode.start_turn, episode.end_turn));

    for episode in ranges {
        let mut messages = Vec::new();
        for turn in turns.iter().filter(|turn| {
            turn.start_turn >= episode.start_turn && turn.end_turn <= episode.end_turn
        }) {
            messages.extend(turn.messages.clone());
        }

        if !messages.is_empty() {
            episodes.push(BoundaryFlushEpisode {
                start_turn: episode.start_turn,
                end_turn: episode.end_turn,
                messages,
            });
        }
    }

    episodes
}

pub(super) fn collect_boundary_episode_for_range(
    entries: Vec<SessionEntry>,
    start_turn: u64,
    end_turn: u64,
) -> Option<BoundaryFlushEpisode> {
    let mut turn_count = 0_u64;
    let mut current_turn_messages = Vec::new();
    let mut current_turn_start = None;
    let mut matched_turns = Vec::new();

    for entry in entries {
        let SessionEntry::Message { message, .. } = entry else {
            continue;
        };

        if message.role == "user" {
            if let Some(turn_start) = current_turn_start.take() {
                matched_turns.push(BoundaryFlushEpisode {
                    start_turn: turn_start,
                    end_turn: turn_start,
                    messages: std::mem::take(&mut current_turn_messages),
                });
            }
            turn_count = turn_count.saturating_add(1);
            current_turn_start = Some(turn_count);
        }

        if current_turn_start.is_some() {
            current_turn_messages.push(message);
        }
    }

    if let Some(turn_start) = current_turn_start.take() {
        matched_turns.push(BoundaryFlushEpisode {
            start_turn: turn_start,
            end_turn: turn_start,
            messages: current_turn_messages,
        });
    }

    let mut messages = Vec::new();
    for turn in matched_turns
        .into_iter()
        .filter(|turn| turn.start_turn >= start_turn && turn.end_turn <= end_turn)
    {
        messages.extend(turn.messages);
    }

    if messages.is_empty() {
        None
    } else {
        Some(BoundaryFlushEpisode {
            start_turn,
            end_turn,
            messages,
        })
    }
}

pub(super) fn split_boundary_flush_episode_batches(
    episodes: &[BoundaryFlushEpisode],
    max_turns_per_batch: u64,
) -> Vec<Vec<BoundaryFlushEpisode>> {
    if episodes.is_empty() {
        return Vec::new();
    }

    let mut batches = Vec::new();
    let mut current_batch = Vec::new();
    let mut current_turns = 0_u64;

    for episode in episodes {
        let episode_turns = episode
            .end_turn
            .saturating_sub(episode.start_turn)
            .saturating_add(1)
            .max(1);

        if !current_batch.is_empty()
            && current_turns.saturating_add(episode_turns) > max_turns_per_batch
        {
            batches.push(current_batch);
            current_batch = Vec::new();
            current_turns = 0;
        }

        current_turns = current_turns.saturating_add(episode_turns);
        current_batch.push(episode.clone());
    }

    if !current_batch.is_empty() {
        batches.push(current_batch);
    }

    batches
}

impl Orchestrator {
    /// Handle the flow after a /reset or /new command.
    /// This creates a fresh session and injects the post-reset prompt to guide the agent.
    pub(super) async fn handle_post_reset_flow(
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

    pub(super) async fn capture_boundary_flush_snapshot(
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

    pub(super) async fn close_open_episodes_for_session_end(
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

    pub(super) async fn update_boundary_flush_state(
        &self,
        agent_id: &str,
        session: &Session,
        turn_count: Option<u64>,
        success: bool,
    ) {
        Self::persist_boundary_flush_state(&self.memory, agent_id, session, turn_count, success)
            .await;
    }

    pub(super) async fn persist_boundary_flush_state(
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

    pub(super) async fn record_session_turn_episode(
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

    pub(super) async fn capture_closed_episode_snapshot(
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

    pub(super) async fn update_closed_episode_flush_state(
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

    pub(super) async fn spawn_closed_episode_flush(
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

    pub(super) async fn schedule_session_end_flush(
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

    pub(super) async fn finalize_stale_boundary_flush(
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

    pub(super) async fn recover_pending_boundary_flushes_for_session_key(
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

    pub(super) async fn schedule_stale_boundary_flush(
        &self,
        view: Arc<ConfigView>,
        agent_id: &str,
        session: &Session,
        agent: &FullAgentConfig,
    ) {
        self.schedule_stale_boundary_flush_with_guard(view, agent_id, session, agent, None)
            .await;
    }

    pub(super) async fn schedule_stale_boundary_flush_with_guard(
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

    pub(super) async fn run_boundary_flush_snapshot(
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

    pub(super) async fn handle_explicit_session_reset(
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
