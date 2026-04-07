use std::sync::Arc;

use clawhive_memory::embedding::EmbeddingProvider;
use clawhive_memory::fact_store::FactStore;
use clawhive_memory::memory_lineage::generate_canonical_id_with_key;
use clawhive_memory::memory_lineage::MemoryLineageStore;
use clawhive_memory::search_index::SearchIndex;
use clawhive_memory::{MemoryStore, RecentExplicitMemoryWrite, SessionMessage};
use clawhive_provider::{ContentBlock, LlmMessage, LlmResponse};

use crate::config::FullAgentConfig;
use crate::memory_document::MemoryDocument;
use crate::memory_retrieval::is_matching_memory_content;
use crate::memory_summary::{
    build_summary_prompt, group_daily_candidates, merge_daily_blocks, parse_candidates,
    retain_summary_candidates, SummaryClass,
};
use crate::router::LlmRouter;
use crate::session::Session;

use crate::config_view::ConfigView;
use crate::session::SessionResetReason;
use clawhive_memory::dirty_sources::{DirtySourceStore, DIRTY_KIND_DAILY_FILE, DIRTY_KIND_SESSION};

use super::episode::find_boundary_flush_conflict;
use super::Orchestrator;

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

pub(crate) fn fact_token_overlap_ratio(a: &str, b: &str) -> f64 {
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

impl Orchestrator {
    pub(super) async fn generate_summary_from_messages_static(
        request: SummaryGenerationRequest<'_>,
    ) -> bool {
        let SummaryGenerationRequest {
            router,
            file_store,
            memory,
            embedding_provider,
            agent_id,
            session,
            agent,
            source,
            messages,
            recent_explicit_writes,
        } = request;
        if messages.is_empty() {
            return false;
        }
        let reader = clawhive_memory::session::SessionReader::new(file_store.workspace_dir());

        // Use the last message's timestamp to determine the daily file date,
        // so that sessions from older dates don't pollute today's daily file.
        let today = messages
            .iter()
            .rev()
            .find_map(|m| m.timestamp)
            .map(|ts| ts.date_naive())
            .unwrap_or_else(|| chrono::Utc::now().date_naive());

        let conversation = messages
            .iter()
            .map(|m| format!("{}: {}", m.role, m.content))
            .collect::<Vec<_>>()
            .join("\n");

        let system = build_summary_prompt();

        let llm_messages = vec![LlmMessage::user(conversation)];

        match router
            .chat(
                &agent.model_policy.primary,
                &agent.model_policy.fallbacks,
                Some(system),
                llm_messages,
                1024,
            )
            .await
        {
            Ok(resp) => {
                let Some(candidates) = parse_candidates(&resp.text) else {
                    let parse_error = crate::memory_summary::parse_candidates_error(&resp.text);
                    tracing::warn!(
                        source,
                        raw_len = resp.text.len(),
                        raw_preview = %resp.text.chars().take(300).collect::<String>(),
                        %parse_error,
                        "Failed to parse structured session summary JSON"
                    );
                    return false;
                };
                let retained = retain_summary_candidates(candidates);
                let fact_store = FactStore::new(memory.db());
                let lineage_store = MemoryLineageStore::new(memory.db());
                let mut active_facts = match fact_store.get_active_facts(agent_id).await {
                    Ok(facts) => facts,
                    Err(error) => {
                        tracing::warn!(source, %error, "Failed to load active facts for summary precheck");
                        Vec::new()
                    }
                };
                let existing_memory_items = match file_store.read_long_term().await {
                    Ok(long_term) if !long_term.trim().is_empty() => {
                        let doc = MemoryDocument::parse(&long_term);
                        crate::memory_document::MEMORY_SECTION_ORDER
                            .iter()
                            .flat_map(|heading| doc.section_items(heading))
                            .collect::<Vec<_>>()
                    }
                    Ok(_) => Vec::new(),
                    Err(error) => {
                        tracing::warn!(
                            source,
                            %error,
                            "Failed to load long-term memory items for summary precheck"
                        );
                        Vec::new()
                    }
                };

                for candidate in &retained.facts {
                    let duplicate_key = normalized_duplicate_key(candidate);
                    let already_recorded = crate::memory_retrieval::find_matching_fact(
                        &active_facts,
                        &candidate.content,
                    )
                    .is_some()
                        || recent_explicit_writes.iter().any(|marker| {
                            is_matching_memory_content(&marker.summary, &candidate.content)
                        })
                        || existing_memory_items
                            .iter()
                            .any(|item| is_matching_memory_content(item, &candidate.content))
                        || if let Some(duplicate_key) = duplicate_key.as_deref() {
                            let canonical_id = generate_canonical_id_with_key(
                                agent_id,
                                "fact",
                                Some(duplicate_key),
                                &candidate.content,
                            );
                            lineage_store
                                .get_canonical(&canonical_id)
                                .await
                                .ok()
                                .flatten()
                                .is_some()
                        } else {
                            false
                        };
                    if already_recorded {
                        continue;
                    }

                    let now = chrono::Utc::now().to_rfc3339();
                    let affect = candidate
                        .affect
                        .clone()
                        .unwrap_or_else(|| "neutral".to_string());
                    let affect_intensity = f64::from(candidate.affect_intensity.unwrap_or(0.0));
                    // Salience boost is applied once inside record_add via
                    // apply_affect_salience_boost; pass raw default here.
                    let salience = 50_u8;
                    let fact = clawhive_memory::fact_store::Fact {
                        id: clawhive_memory::fact_store::generate_fact_id(
                            agent_id,
                            &candidate.content,
                        ),
                        agent_id: agent_id.to_string(),
                        content: candidate.content.clone(),
                        fact_type: normalized_candidate_fact_type(candidate).to_string(),
                        importance: f64::from(candidate.importance.clamp(0.0, 1.0)),
                        confidence: 0.9,
                        status: "active".to_string(),
                        occurred_at: None,
                        recorded_at: now.clone(),
                        source_type: "boundary_flush".to_string(),
                        source_session: Some(session.session_id.clone()),
                        access_count: 0,
                        last_accessed: None,
                        superseded_by: None,
                        salience,
                        supersede_reason: None,
                        affect,
                        affect_intensity,
                        created_at: now.clone(),
                        updated_at: now,
                    };
                    let conflict = match find_boundary_flush_conflict(
                        embedding_provider,
                        &fact.content,
                        &fact.fact_type,
                        &active_facts,
                    )
                    .await
                    {
                        Ok(conflict) => conflict,
                        Err(error) => {
                            tracing::warn!(
                                source,
                                content = %candidate.content,
                                error = %error,
                                "Boundary flush conflict check failed; proceeding with insert fallback"
                            );
                            None
                        }
                    };

                    if let Some(old_fact) = conflict {
                        let new_fact_id = fact.id.clone();
                        match fact_store
                            .supersede(&old_fact.id, &fact, "auto_boundary_flush")
                            .await
                        {
                            Ok(()) => {
                                let _ = fact_store.record_add(&fact).await;
                                active_facts.retain(|existing| existing.id != old_fact.id);
                                active_facts.push(fact);
                                tracing::info!(
                                    source,
                                    old_fact_id = %old_fact.id,
                                    new_fact_id = %new_fact_id,
                                    reason = "auto_boundary_flush",
                                    "Auto-superseded conflicting boundary fact"
                                );
                            }
                            Err(error) => {
                                tracing::warn!(
                                    source,
                                    content = %candidate.content,
                                    old_fact_id = %old_fact.id,
                                    error = %error,
                                    "Failed to auto-supersede boundary fact candidate"
                                );
                            }
                        }
                        continue;
                    }

                    match fact_store
                        .insert_fact_with_canonical_key(&fact, duplicate_key.as_deref())
                        .await
                    {
                        Ok(()) => {
                            let _ = fact_store.record_add(&fact).await;
                            active_facts.push(fact);
                        }
                        Err(error) => {
                            tracing::warn!(
                                source,
                                content = %candidate.content,
                                error = %error,
                                "Failed to persist boundary fact candidate"
                            );
                        }
                    }
                }

                let mut retained_for_daily = Vec::new();
                for mut candidate in retained.daily.iter().chain(retained.memory.iter()).cloned() {
                    if crate::memory_retrieval::find_matching_fact(
                        &active_facts,
                        &candidate.content,
                    )
                    .is_some()
                    {
                        continue;
                    }
                    if recent_explicit_writes.iter().any(|marker| {
                        is_matching_memory_content(&marker.summary, &candidate.content)
                    }) {
                        continue;
                    }
                    if existing_memory_items
                        .iter()
                        .any(|item| is_matching_memory_content(item, &candidate.content))
                    {
                        continue;
                    }

                    let duplicate_hit =
                        if let Some(duplicate_key) = normalized_duplicate_key(&candidate) {
                            let daily_canonical_id = generate_canonical_id_with_key(
                                agent_id,
                                "daily",
                                Some(duplicate_key.as_str()),
                                &candidate.content,
                            );
                            let daily_exists = lineage_store
                                .get_canonical(&daily_canonical_id)
                                .await
                                .ok()
                                .flatten()
                                .is_some();

                            if daily_exists {
                                true
                            } else if candidate.classification == SummaryClass::Memory {
                                let memory_canonical_id = generate_canonical_id_with_key(
                                    agent_id,
                                    "memory",
                                    Some(duplicate_key.as_str()),
                                    &candidate.content,
                                );
                                lineage_store
                                    .get_canonical(&memory_canonical_id)
                                    .await
                                    .ok()
                                    .flatten()
                                    .is_some()
                            } else {
                                false
                            }
                        } else {
                            false
                        };
                    if duplicate_hit {
                        continue;
                    }

                    candidate.classification = SummaryClass::Daily;
                    retained_for_daily.push(candidate);
                }
                if retained_for_daily.is_empty() {
                    tracing::info!(source, "No daily-worthy summary candidates retained");
                    return true;
                }
                let grouped = group_daily_candidates(&retained_for_daily);
                let grouped_for_write = grouped.clone();
                let rendered = match file_store
                    .update_daily(today, move |existing| {
                        Ok(merge_daily_blocks(
                            today,
                            existing.as_deref(),
                            &grouped_for_write,
                        ))
                    })
                    .await
                {
                    Ok(rendered) => rendered,
                    Err(error) => {
                        tracing::warn!(source, %error, "Failed to update daily file");
                        return false;
                    }
                };
                let Some(_rendered) = rendered else {
                    tracing::info!(
                        source,
                        "Structured session summary produced no daily changes"
                    );
                    return true;
                };
                {
                    let relative_path = format!("memory/{}.md", today.format("%Y-%m-%d"));
                    let dirty = DirtySourceStore::new(memory.db());
                    let mut daily_reindexed = false;
                    let session_path_prefix = format!("sessions/{}#", session.session_id);

                    for candidate in &retained_for_daily {
                        let canonical_key = candidate
                            .duplicate_key
                            .as_deref()
                            .map(str::trim)
                            .filter(|value| !value.is_empty());
                        let canonical = match lineage_store
                            .ensure_canonical_with_key(
                                agent_id,
                                "daily",
                                canonical_key,
                                &candidate.content,
                            )
                            .await
                        {
                            Ok(canonical) => canonical,
                            Err(error) => {
                                tracing::warn!(
                                    source,
                                    content = %candidate.content,
                                    error = %error,
                                    "Failed to ensure daily canonical for pre-reindex session linkage"
                                );
                                continue;
                            }
                        };

                        if let Err(error) = lineage_store
                            .attach_matching_chunks_by_prefix(
                                agent_id,
                                &canonical.canonical_id,
                                &session_path_prefix,
                                &candidate.content,
                                "raw",
                            )
                            .await
                        {
                            tracing::warn!(
                                source,
                                canonical_id = %canonical.canonical_id,
                                error = %error,
                                "Failed to pre-link session chunk lineage for daily candidate"
                            );
                        }
                    }

                    match dirty
                        .enqueue(agent_id, DIRTY_KIND_DAILY_FILE, &relative_path, source)
                        .await
                    {
                        Err(error) => {
                            tracing::warn!(source, %error, "Failed to enqueue daily dirty source");
                        }
                        _ => {
                            let search_index = SearchIndex::new(memory.db(), agent_id);
                            match search_index
                                .process_dirty_sources(
                                    &dirty,
                                    agent_id,
                                    file_store,
                                    &reader,
                                    embedding_provider.as_ref(),
                                    4,
                                )
                                .await
                            {
                                Err(error) => {
                                    tracing::warn!(source, %error, "Failed to drain daily dirty source");
                                }
                                _ => {
                                    daily_reindexed = true;
                                }
                            }
                        }
                    }

                    for candidate in &retained_for_daily {
                        let canonical_key = candidate
                            .duplicate_key
                            .as_deref()
                            .map(str::trim)
                            .filter(|value| !value.is_empty());
                        let canonical = match lineage_store
                            .ensure_canonical_with_key(
                                agent_id,
                                "daily",
                                canonical_key,
                                &candidate.content,
                            )
                            .await
                        {
                            Ok(canonical) => canonical,
                            Err(error) => {
                                tracing::warn!(
                                    source,
                                    content = %candidate.content,
                                    error = %error,
                                    "Failed to ensure daily canonical"
                                );
                                continue;
                            }
                        };

                        if let Err(error) = lineage_store
                            .attach_source(
                                agent_id,
                                &canonical.canonical_id,
                                "daily_section",
                                &format!("{relative_path}#{}", canonical.canonical_id),
                                "summary",
                            )
                            .await
                        {
                            tracing::warn!(
                                source,
                                canonical_id = %canonical.canonical_id,
                                error = %error,
                                "Failed to record daily section lineage"
                            );
                        }

                        match lineage_store
                            .attach_matching_chunks_by_prefix(
                                agent_id,
                                &canonical.canonical_id,
                                &session_path_prefix,
                                &candidate.content,
                                "raw",
                            )
                            .await
                        {
                            Ok(0) => {
                                if let Err(error) = lineage_store
                                    .attach_matching_chunks_by_prefix(
                                        agent_id,
                                        &canonical.canonical_id,
                                        "sessions/",
                                        &candidate.content,
                                        "raw",
                                    )
                                    .await
                                {
                                    tracing::warn!(
                                        source,
                                        canonical_id = %canonical.canonical_id,
                                        error = %error,
                                        "Failed to record fallback session chunk lineage for daily candidate"
                                    );
                                }
                            }
                            Ok(_) => {}
                            Err(error) => {
                                tracing::warn!(
                                    source,
                                    canonical_id = %canonical.canonical_id,
                                    error = %error,
                                    "Failed to record session chunk lineage for daily candidate"
                                );
                            }
                        }

                        if daily_reindexed {
                            if let Err(error) = lineage_store
                                .attach_matching_chunks(
                                    agent_id,
                                    &canonical.canonical_id,
                                    &relative_path,
                                    &candidate.content,
                                    "summary",
                                )
                                .await
                            {
                                tracing::warn!(
                                    source,
                                    canonical_id = %canonical.canonical_id,
                                    error = %error,
                                    "Failed to record daily chunk lineage for daily candidate"
                                );
                            }
                        }
                    }
                    tracing::info!(
                        source,
                        blocks = grouped.len(),
                        "Wrote structured session summary"
                    );
                    let topics = grouped
                        .iter()
                        .map(|block| block.topic.clone())
                        .collect::<Vec<String>>();
                    memory
                        .record_trace(
                            agent_id,
                            "write",
                            &serde_json::json!({
                                "source": source,
                                "target": format!("memory/{}.md", today.format("%Y-%m-%d")),
                                "topics": topics,
                            })
                            .to_string(),
                            None,
                        )
                        .await;
                }
                true
            }
            Err(e) => {
                tracing::warn!("Failed to generate {source}: {e}");
                false
            }
        }
    }

    pub(super) async fn try_fallback_summary(
        &self,
        view: &ConfigView,
        agent_id: &str,
        session: &Session,
        agent: &FullAgentConfig,
        reason: SessionResetReason,
    ) {
        let Some(snapshot) = self
            .capture_boundary_flush_snapshot(agent_id, session, agent)
            .await
        else {
            self.update_boundary_flush_state(agent_id, session, None, true)
                .await;
            return;
        };
        let success = self
            .run_boundary_flush_snapshot(
                view,
                agent_id,
                session,
                agent,
                "fallback_summary",
                snapshot,
            )
            .await;
        if matches!(reason, SessionResetReason::Idle | SessionResetReason::Daily) {
            if success {
                let workspace = self.workspace_state_for(agent_id);
                match workspace
                    .session_writer
                    .archive_session(&session.session_id)
                    .await
                {
                    Err(error) => {
                        tracing::warn!(
                            %error,
                            %agent_id,
                            session_key = %session.session_key.0,
                            session_id = %session.session_id,
                            "Failed to archive stale session transcript after boundary flush"
                        );
                    }
                    _ => {
                        let _ = self
                            .memory
                            .delete_session_memory_state(agent_id, &session.session_id)
                            .await;
                        self.enqueue_dirty_source(
                            agent_id,
                            DIRTY_KIND_SESSION,
                            &session.session_id,
                            "session_archived_after_reset",
                        )
                        .await;
                        self.drain_dirty_sources(view, agent_id, 8).await;
                    }
                }
            } else {
                tracing::warn!(
                    %agent_id,
                    session_key = %session.session_key.0,
                    session_id = %session.session_id,
                    "Boundary flush failed for stale session; keeping transcript in place to avoid losing retry source"
                );
            }
        } else if !success {
            tracing::warn!(
                %agent_id,
                session_key = %session.session_key.0,
                session_id = %session.session_id,
                "Boundary flush failed before explicit reset"
            );
        }
    }
}

pub(super) fn detect_empty_promise_structural(
    retry_count: u32,
    current_tool_calls: usize,
    response_text: &str,
) -> EmptyPromiseVerdict {
    if retry_count >= 2 || current_tool_calls > 0 {
        return EmptyPromiseVerdict::No;
    }

    let trimmed = response_text.trim();
    if trimmed.len() >= 500 {
        return EmptyPromiseVerdict::No;
    }

    let ends_with_continuation = trimmed.ends_with(':')
        || trimmed.ends_with('\u{ff1a}') // ：
        || trimmed.ends_with("——")
        || trimmed.ends_with("—")
        || trimmed.ends_with("...")
        || trimmed.ends_with('\u{2026}') // …
        || trimmed.ends_with("\u{2026}\u{2026}"); // ……

    if ends_with_continuation {
        return EmptyPromiseVerdict::Structural;
    }

    let ends_with_sentence_ending = trimmed.ends_with('.')
        || trimmed.ends_with('!')
        || trimmed.ends_with('?')
        || trimmed.ends_with('\u{3002}') // 。
        || trimmed.ends_with('\u{ff01}') // ！
        || trimmed.ends_with('\u{ff1f}') // ？
        || trimmed.ends_with('"')
        || trimmed.ends_with('\u{201d}') // "
        || trimmed.ends_with(')')
        || trimmed.ends_with('\u{ff09}'); // ）

    if ends_with_sentence_ending {
        return EmptyPromiseVerdict::No;
    }

    EmptyPromiseVerdict::Inconclusive
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EmptyPromiseVerdict {
    Structural,
    Inconclusive,
    No,
}

pub(super) async fn detect_empty_promise_by_llm(
    router: &LlmRouter,
    primary: &str,
    fallbacks: &[String],
    response_text: &str,
) -> bool {
    let prompt = format!(
        "An AI assistant produced the following response to a user:\n\
         ---\n{response_text}\n---\n\
         Did the assistant promise or announce that it would produce content \
         (compile, write, generate, summarize, etc.) without actually providing \
         that content in the response? Answer only YES or NO."
    );
    let result = router
        .chat(
            primary,
            fallbacks,
            Some("You are a binary classifier. Answer only YES or NO.".to_string()),
            vec![LlmMessage::user(prompt)],
            16,
        )
        .await;
    match result {
        Ok(resp) => resp.text.trim().to_uppercase().starts_with("YES"),
        Err(e) => {
            tracing::warn!("empty promise LLM detection failed, skipping: {e}");
            false
        }
    }
}

pub(super) fn synthesize_cancelled_response(tool_summaries: &[(String, String)]) -> LlmResponse {
    let text = if tool_summaries.is_empty() {
        "[Task stopped by user]".to_string()
    } else {
        let mut message = "[Task stopped by user]\n\nCompleted:".to_string();
        for (name, preview) in tool_summaries {
            message.push_str(&format!("\n- {name}: {preview}"));
        }
        message
    };

    LlmResponse {
        text: text.clone(),
        content: vec![ContentBlock::Text { text }],
        input_tokens: None,
        output_tokens: None,
        stop_reason: Some("cancelled".into()),
    }
}
