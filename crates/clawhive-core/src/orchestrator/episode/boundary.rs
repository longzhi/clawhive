use std::collections::HashSet;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use chrono::Utc;
use clawhive_memory::embedding::EmbeddingProvider;
use clawhive_memory::{
    EpisodeStateRecord, EpisodeStatusRecord, EpisodeTaskStateRecord, SessionEntry, SessionMessage,
};

use crate::orchestrator::summary::{contains_correction_phrase, fact_token_overlap_ratio};
use crate::orchestrator::{detect_empty_promise_structural, EmptyPromiseVerdict};

use super::types::{BoundaryFlushEpisode, EpisodeBoundaryDecision, EpisodeTurnDecision};

pub(in crate::orchestrator) const EPISODE_FLUSH_PENDING_GRACE_SECS: i64 = 30;
pub(in crate::orchestrator) const MAX_OPEN_EPISODE_TURNS: u64 = 4;
pub(in crate::orchestrator) const MAX_BOUNDARY_FLUSH_TURNS_PER_BATCH: u64 = 50;

pub(in crate::orchestrator) fn boundary_flush_conflict_passes_two_step(
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

pub(in crate::orchestrator) fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
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

pub(in crate::orchestrator) fn boundary_flush_topic_tokens(
    messages: &[SessionMessage],
) -> HashSet<String> {
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

pub(in crate::orchestrator) fn boundary_flush_topic_tokens_from_text(
    text: &str,
) -> HashSet<String> {
    text.split(|ch: char| !ch.is_alphanumeric())
        .map(str::trim)
        .filter(|token| token.len() >= 3)
        .map(|token| token.to_ascii_lowercase())
        .collect()
}

pub(in crate::orchestrator) fn build_episode_topic_sketch(text: &str) -> String {
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

pub(in crate::orchestrator) fn boundary_flush_topics_are_related(
    current: &HashSet<String>,
    next: &HashSet<String>,
) -> bool {
    if current.is_empty() || next.is_empty() {
        return false;
    }

    current.intersection(next).count() >= 2
}

pub(in crate::orchestrator) fn infer_episode_task_state(
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

pub(in crate::orchestrator) fn decide_episode_turn(
    current_tokens: &HashSet<String>,
    next_tokens: &HashSet<String>,
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

pub(in crate::orchestrator) fn episode_status_ready_for_boundary_flush(
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

pub(in crate::orchestrator) fn collect_unflushed_boundary_turns(
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

pub(in crate::orchestrator) fn collect_unflushed_boundary_episodes(
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

pub(in crate::orchestrator) fn build_boundary_episodes_from_state(
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

pub(in crate::orchestrator) fn collect_boundary_episode_for_range(
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

pub(in crate::orchestrator) fn split_boundary_flush_episode_batches(
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
