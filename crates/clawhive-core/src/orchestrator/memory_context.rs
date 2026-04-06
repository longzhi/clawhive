use anyhow::Result;
use clawhive_schema::SessionKey;

use crate::config_view::ConfigView;
use crate::memory_retrieval::{
    filter_duplicate_chunks_against_facts, infer_memory_routing_bias, search_memory, MemoryHit,
    MemoryRoutingBias, MemorySearchParams, MemorySourceKind,
};

use super::Orchestrator;

impl Orchestrator {
    pub(super) async fn build_memory_context(
        &self,
        view: &ConfigView,
        agent_id: &str,
        _session_key: &SessionKey,
        query: &str,
    ) -> Result<String> {
        let budget = view
            .agent(agent_id)
            .and_then(|agent| agent.memory_policy.as_ref())
            .map(|policy| policy.max_injected_chars)
            .unwrap_or(6000);

        let fact_store = clawhive_memory::fact_store::FactStore::new(self.memory.db());
        let facts = fact_store
            .get_injected_facts(agent_id)
            .await
            .unwrap_or_default();

        let search_start = std::time::Instant::now();
        let results = search_memory(
            &fact_store,
            &self.search_index_for(agent_id),
            view.embedding_provider.as_ref(),
            agent_id,
            query,
            MemorySearchParams {
                max_results: 6,
                min_score: 0.25,
                time_range: None,
            },
        )
        .await;
        let search_ms = search_start.elapsed().as_millis() as i64;

        match results {
            Ok(results) if !results.is_empty() => {
                let matched_facts = results
                    .iter()
                    .filter_map(|hit| match hit {
                        MemoryHit::Fact(hit) => Some(hit.fact.clone()),
                        MemoryHit::Chunk(_) => None,
                    })
                    .collect::<Vec<_>>();
                let routing_bias = infer_memory_routing_bias(&results);
                let context = if should_use_long_term_fallback(routing_bias, &results) {
                    let long_term = self.file_store_for(agent_id).read_long_term().await?;
                    if long_term.trim().is_empty() {
                        build_memory_context_from_hits(&results, budget)
                    } else {
                        build_memory_context_from_fallback(&matched_facts, &long_term, budget)
                    }
                } else {
                    build_memory_context_from_hits(&results, budget)
                };
                self.memory.record_trace(
                    agent_id,
                    "search",
                    &serde_json::json!({
                        "query": query.chars().take(200).collect::<String>(),
                        "candidates": results.len(),
                        "scores": results.iter().map(|r| format!("{:.2}", r.score())).collect::<Vec<_>>(),
                        "sources": results.iter().map(|r| format!("{:?}", r.source_kind())).collect::<Vec<_>>(),
                        "score_breakdown": results.iter().filter_map(|hit| match hit {
                            MemoryHit::Chunk(chunk) => chunk.score_breakdown.clone().map(|breakdown| {
                                serde_json::json!({
                                    "chunk_id": chunk.chunk_id.clone(),
                                    "breakdown": breakdown,
                                })
                            }),
                            MemoryHit::Fact(_) => None,
                        }).collect::<Vec<_>>(),
                    }).to_string(),
                    Some(search_ms),
                ).await;
                self.memory
                    .record_trace(
                        agent_id,
                        "inject",
                        &serde_json::json!({
                            "budget": budget,
                            "injected_chars": context.len(),
                            "result_count": results.len(),
                        })
                        .to_string(),
                        None,
                    )
                    .await;
                Ok(context)
            }
            _ => {
                let fallback = self.file_store_for(agent_id).build_memory_context().await?;
                if !fallback.trim().is_empty() {
                    let context = build_memory_context_from_fallback(&facts, &fallback, budget);
                    self.memory
                        .record_trace(
                            agent_id,
                            "inject",
                            &serde_json::json!({
                                "budget": budget,
                                "injected_chars": context.len(),
                                "source": if facts.is_empty() { "fallback" } else { "facts_plus_fallback" },
                            })
                            .to_string(),
                            Some(search_ms),
                        )
                        .await;
                    return Ok(context);
                }

                if facts.is_empty() {
                    return Ok(String::new());
                }

                let context = build_known_facts_section(&facts, budget);
                self.memory
                    .record_trace(
                        agent_id,
                        "inject",
                        &serde_json::json!({
                            "budget": budget,
                            "injected_chars": context.len(),
                            "source": "facts_only",
                        })
                        .to_string(),
                        Some(search_ms),
                    )
                    .await;
                Ok(context)
            }
        }
    }
}

pub(super) fn build_memory_context_from_hits(hits: &[MemoryHit], budget: usize) -> String {
    if hits.is_empty() || budget == 0 {
        return String::new();
    }

    let routing_bias = infer_memory_routing_bias(hits);
    let facts = hits
        .iter()
        .filter_map(|hit| match hit {
            MemoryHit::Fact(hit) => Some(hit.fact.clone()),
            MemoryHit::Chunk(_) => None,
        })
        .collect::<Vec<_>>();
    let chunks = hits
        .iter()
        .filter_map(|hit| match hit {
            MemoryHit::Chunk(hit) => Some(hit.as_ref().clone()),
            MemoryHit::Fact(_) => None,
        })
        .collect::<Vec<_>>();
    let chunks = select_chunks_for_context(
        filter_duplicate_chunks_against_facts(chunks, &facts),
        routing_bias,
    );

    let facts_budget = if facts.is_empty() || chunks.is_empty() {
        budget
    } else {
        match routing_bias {
            MemoryRoutingBias::LongTerm => (budget / 4).min(1500),
            MemoryRoutingBias::ShortTerm => (budget / 6).min(900),
            MemoryRoutingBias::Neutral => (budget / 3).min(1500),
        }
    };
    let facts_section = build_known_facts_section(&facts, facts_budget);
    let facts_chars = facts_section.chars().count();
    let remaining_budget = budget.saturating_sub(facts_chars);
    let chunks_section = clamp_to_budget(&chunks, remaining_budget);

    format!("{facts_section}{chunks_section}")
}

pub(super) fn build_memory_context_from_fallback(
    facts: &[clawhive_memory::fact_store::Fact],
    fallback: &str,
    budget: usize,
) -> String {
    if budget == 0 {
        return String::new();
    }

    let fallback = truncate_text_to_budget(fallback, budget);
    if facts.is_empty() {
        return fallback;
    }
    if fallback.trim().is_empty() {
        return build_known_facts_section(facts, budget);
    }

    let facts_budget = (budget / 3).min(1800);
    let facts_section = build_known_facts_section(facts, facts_budget);
    let remaining_budget = budget.saturating_sub(facts_section.chars().count());
    let fallback_section = truncate_text_to_budget(fallback.trim(), remaining_budget);
    format!("{facts_section}{fallback_section}")
}

pub(super) fn should_use_long_term_fallback(
    routing_bias: MemoryRoutingBias,
    hits: &[MemoryHit],
) -> bool {
    routing_bias != MemoryRoutingBias::ShortTerm
        && !hits.iter().any(|hit| {
            matches!(
                hit.source_kind(),
                MemorySourceKind::Fact | MemorySourceKind::LongTerm
            )
        })
}

pub(super) fn select_chunks_for_context(
    chunks: Vec<clawhive_memory::search_index::SearchResult>,
    routing_bias: MemoryRoutingBias,
) -> Vec<clawhive_memory::search_index::SearchResult> {
    let mut long_term = Vec::new();
    let mut daily = Vec::new();
    let mut session = Vec::new();
    let mut other = Vec::new();

    for chunk in chunks {
        match crate::memory_retrieval::classify_chunk_source(&chunk.source, &chunk.path) {
            MemorySourceKind::LongTerm => long_term.push(chunk),
            MemorySourceKind::Daily => daily.push(chunk),
            MemorySourceKind::Session => session.push(chunk),
            _ => other.push(chunk),
        }
    }

    let has_long_term = !long_term.is_empty();
    let mut selected = Vec::new();
    match routing_bias {
        MemoryRoutingBias::LongTerm => {
            selected.extend(long_term.into_iter().take(4));
            if !has_long_term {
                selected.extend(daily.into_iter().take(3));
                selected.extend(session.into_iter().take(1));
            }
        }
        MemoryRoutingBias::ShortTerm => {
            selected.extend(daily.into_iter().take(4));
            selected.extend(session.into_iter().take(2));
            selected.extend(long_term.into_iter().take(1));
        }
        MemoryRoutingBias::Neutral => {
            selected.extend(long_term.into_iter().take(2));
            selected.extend(daily.into_iter().take(2));
            selected.extend(session.into_iter().take(1));
        }
    }
    selected.extend(other);
    selected
}

pub(super) fn clamp_to_budget(
    results: &[clawhive_memory::search_index::SearchResult],
    budget: usize,
) -> String {
    const HEADER: &str = "## Relevant Memory\n\n";
    const TRUNCATED: &str = "\n...[truncated]";

    if results.is_empty() || budget == 0 {
        return String::new();
    }

    let header_chars = HEADER.chars().count();
    if budget <= header_chars {
        return HEADER.chars().take(budget).collect();
    }

    let mut context = String::from(HEADER);
    let mut used_chars = header_chars;

    for result in results {
        let entry = format!(
            "### {} (score: {:.2})\n{}\n\n",
            result.path, result.score, result.text
        );
        let entry_chars = entry.chars().count();

        if used_chars + entry_chars > budget {
            if used_chars == header_chars {
                let truncated_chars = TRUNCATED.chars().count();
                let available_chars = budget.saturating_sub(used_chars + truncated_chars);
                let truncated: String = entry.chars().take(available_chars).collect();
                context.push_str(&truncated);
                if used_chars + truncated.chars().count() + truncated_chars <= budget {
                    context.push_str(TRUNCATED);
                }
            }
            break;
        }

        context.push_str(&entry);
        used_chars += entry_chars;
    }

    context
}

pub(super) fn truncate_text_to_budget(text: &str, budget: usize) -> String {
    const TRUNCATED: &str = "\n...[truncated]";

    if budget == 0 {
        return String::new();
    }

    if text.chars().count() <= budget {
        return text.to_string();
    }

    let truncated_chars = TRUNCATED.chars().count();
    if budget <= truncated_chars {
        return text.chars().take(budget).collect();
    }

    let available_chars = budget.saturating_sub(truncated_chars);
    let truncated: String = text.chars().take(available_chars).collect();
    format!("{truncated}{TRUNCATED}")
}

pub(super) fn build_known_facts_section(
    facts: &[clawhive_memory::fact_store::Fact],
    budget: usize,
) -> String {
    if facts.is_empty() || budget == 0 {
        return String::new();
    }

    let mut section = String::from("## Known Facts\n\n");
    for fact in facts {
        section.push_str(&format!("- [{}] {}\n", fact.fact_type, fact.content));
    }
    section.push('\n');

    truncate_text_to_budget(&section, budget)
}

pub(super) fn truncate_tool_result_preview(text: &str, max_chars: usize) -> String {
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() || normalized.chars().count() <= max_chars {
        return normalized;
    }

    let truncated: String = normalized.chars().take(max_chars).collect();
    format!("{truncated}…")
}
