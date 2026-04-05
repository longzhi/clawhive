use std::collections::{HashMap, HashSet};

use anyhow::Result;
use clawhive_memory::embedding::EmbeddingProvider;
use clawhive_memory::fact_store::{Fact, FactStore};
use clawhive_memory::memory_lineage::MemoryLineageStore;
use clawhive_memory::search_index::{SearchIndex, SearchResult, TimeRange};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemorySourceKind {
    Fact,
    LongTerm,
    Daily,
    Session,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryRoutingBias {
    LongTerm,
    ShortTerm,
    Neutral,
}

#[derive(Debug, Clone)]
pub struct MemorySearchParams {
    pub max_results: usize,
    pub min_score: f64,
    pub time_range: Option<TimeRange>,
}

#[derive(Debug, Clone)]
pub struct MemoryFactHit {
    pub fact: Fact,
    pub score: f64,
}

#[derive(Debug, Clone)]
pub enum MemoryHit {
    Fact(Box<MemoryFactHit>),
    Chunk(Box<SearchResult>),
}

impl MemoryHit {
    pub fn score(&self) -> f64 {
        match self {
            Self::Fact(hit) => hit.score,
            Self::Chunk(hit) => hit.score,
        }
    }

    pub fn source_kind(&self) -> MemorySourceKind {
        match self {
            Self::Fact(_) => MemorySourceKind::Fact,
            Self::Chunk(hit) => classify_chunk_source(&hit.source, &hit.path),
        }
    }

    pub fn content(&self) -> &str {
        match self {
            Self::Fact(hit) => &hit.fact.content,
            Self::Chunk(hit) => &hit.text,
        }
    }

    pub fn chunk_id(&self) -> Option<&str> {
        match self {
            Self::Fact(_) => None,
            Self::Chunk(hit) => Some(&hit.chunk_id),
        }
    }

    pub fn fact_id(&self) -> Option<&str> {
        match self {
            Self::Fact(hit) => Some(&hit.fact.id),
            Self::Chunk(_) => None,
        }
    }

    fn apply_source_bias(&mut self, bias: MemoryRoutingBias) {
        let factor = source_weight(self.source_kind(), bias);
        match self {
            Self::Fact(hit) => hit.score *= factor,
            Self::Chunk(hit) => hit.score *= factor,
        }
    }
}

pub async fn search_memory(
    fact_store: &FactStore,
    search_index: &SearchIndex,
    provider: &dyn EmbeddingProvider,
    agent_id: &str,
    query: &str,
    params: MemorySearchParams,
) -> Result<Vec<MemoryHit>> {
    let target_results = if params.max_results == 0 {
        6
    } else {
        params.max_results
    };
    let facts = fact_store.get_active_facts(agent_id).await?;
    let filtered_facts = filter_facts_by_time_range(&facts, params.time_range.as_ref());
    let mut hits = score_facts(
        &filtered_facts,
        query,
        params.min_score,
        MemoryRoutingBias::Neutral,
    )
    .into_iter()
    .map(|hit| MemoryHit::Fact(Box::new(hit)))
    .collect::<Vec<_>>();

    let mut chunks = search_index
        .search(
            query,
            provider,
            target_results.saturating_mul(3),
            params.min_score,
            params.time_range,
        )
        .await?;
    rerank_chunks_by_source(&mut chunks, MemoryRoutingBias::Neutral);
    hits.extend(
        chunks
            .into_iter()
            .map(|chunk| MemoryHit::Chunk(Box::new(chunk))),
    );
    hits.sort_by(|a, b| b.score().total_cmp(&a.score()));
    let chunk_canonical_ids = load_chunk_canonical_ids(fact_store, agent_id, &hits)
        .await
        .unwrap_or_else(|error| {
            tracing::warn!(
                agent_id = %agent_id,
                error = %error,
                "failed to load chunk canonical ids for memory retrieval"
            );
            HashMap::new()
        });
    let fact_canonical_ids = load_fact_canonical_ids(fact_store, agent_id, &hits)
        .await
        .unwrap_or_else(|error| {
            tracing::warn!(
                agent_id = %agent_id,
                error = %error,
                "failed to load fact canonical ids for memory retrieval"
            );
            HashMap::new()
        });
    let superseding_canonical_ids =
        load_superseding_chunk_canonical_ids(fact_store, agent_id, &chunk_canonical_ids)
            .await
            .unwrap_or_else(|error| {
                tracing::warn!(
                    agent_id = %agent_id,
                    error = %error,
                    "failed to load superseding chunk canonical ids for memory retrieval"
                );
                HashMap::new()
            });
    hits = dedup_memory_hits_with_chunk_canonicals_and_supersedes(
        hits,
        &fact_canonical_ids,
        &chunk_canonical_ids,
        &superseding_canonical_ids,
    );
    let routing_bias = infer_memory_routing_bias(&hits);
    if routing_bias != MemoryRoutingBias::Neutral {
        for hit in &mut hits {
            hit.apply_source_bias(routing_bias);
        }
        hits.sort_by(|a, b| b.score().total_cmp(&a.score()));
        hits = dedup_memory_hits_with_chunk_canonicals_and_supersedes(
            hits,
            &fact_canonical_ids,
            &chunk_canonical_ids,
            &superseding_canonical_ids,
        );
    }
    hits.truncate(target_results);

    // Bump access_count for returned facts (fire-and-forget)
    let returned_fact_ids: Vec<String> = hits
        .iter()
        .filter_map(|h| match h {
            MemoryHit::Fact(f) => Some(f.fact.id.clone()),
            _ => None,
        })
        .collect();
    if !returned_fact_ids.is_empty() {
        let fact_store = fact_store.clone();
        tokio::spawn(async move {
            let _ = fact_store.bump_access(&returned_fact_ids).await;
        });
    }

    Ok(hits)
}

fn filter_facts_by_time_range(facts: &[Fact], time_range: Option<&TimeRange>) -> Vec<Fact> {
    let Some(range) = time_range else {
        return facts.to_vec();
    };

    let from = range
        .from
        .as_deref()
        .and_then(|value| parse_time_boundary(value, true));
    let to = range
        .to
        .as_deref()
        .and_then(|value| parse_time_boundary(value, false));

    facts
        .iter()
        .filter(|fact| {
            let Some(occurred_at) = fact.occurred_at.as_deref() else {
                return false;
            };
            let Some(date) = parse_fact_occurred_date(occurred_at) else {
                return false;
            };
            if let Some(from_date) = from {
                if date < from_date {
                    return false;
                }
            }
            if let Some(to_date) = to {
                if date > to_date {
                    return false;
                }
            }
            true
        })
        .cloned()
        .collect()
}

fn parse_fact_occurred_date(value: &str) -> Option<chrono::NaiveDate> {
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|dt| dt.date_naive())
        .ok()
        .or_else(|| chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d").ok())
}

fn parse_time_boundary(value: &str, is_start: bool) -> Option<chrono::NaiveDate> {
    if let Ok(date) = chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d") {
        return Some(date);
    }

    let (year, month) = value.split_once('-')?;
    if month.len() != 2 || year.len() != 4 || value.matches('-').count() != 1 {
        return None;
    }

    let year: i32 = year.parse().ok()?;
    let month: u32 = month.parse().ok()?;
    let start = chrono::NaiveDate::from_ymd_opt(year, month, 1)?;
    if is_start {
        return Some(start);
    }

    let (next_year, next_month) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    let next_start = chrono::NaiveDate::from_ymd_opt(next_year, next_month, 1)?;
    Some(next_start - chrono::Duration::days(1))
}

pub fn dedup_memory_hits(hits: Vec<MemoryHit>) -> Vec<MemoryHit> {
    dedup_memory_hits_with_chunk_canonicals(hits, &HashMap::new())
}

pub fn dedup_memory_hits_with_chunk_canonicals(
    hits: Vec<MemoryHit>,
    chunk_canonical_ids: &HashMap<String, Vec<String>>,
) -> Vec<MemoryHit> {
    dedup_memory_hits_with_chunk_canonicals_and_supersedes(
        hits,
        &HashMap::new(),
        chunk_canonical_ids,
        &HashMap::new(),
    )
}

pub fn dedup_memory_hits_with_chunk_canonicals_and_supersedes(
    hits: Vec<MemoryHit>,
    fact_canonical_ids: &HashMap<String, Vec<String>>,
    chunk_canonical_ids: &HashMap<String, Vec<String>>,
    superseding_canonical_ids: &HashMap<String, Vec<String>>,
) -> Vec<MemoryHit> {
    let mut selected = Vec::new();
    let mut normalized_selected: Vec<String> = Vec::new();
    let mut selected_canonical_ids = HashSet::new();
    let present_fact_canonical_ids = fact_canonical_ids
        .values()
        .flat_map(|canonical_ids| canonical_ids.iter().cloned())
        .collect::<HashSet<_>>();
    let present_canonical_ids = chunk_canonical_ids
        .values()
        .flat_map(|canonical_ids| canonical_ids.iter().cloned())
        .collect::<HashSet<_>>();

    for hit in hits {
        let chunk_canonicals = hit
            .chunk_id()
            .and_then(|chunk_id| chunk_canonical_ids.get(chunk_id));
        if chunk_canonicals.is_some_and(|canonical_ids| {
            canonical_ids
                .iter()
                .any(|canonical_id| present_fact_canonical_ids.contains(canonical_id))
        }) {
            continue;
        }
        if chunk_canonicals.is_some_and(|canonical_ids| {
            !canonical_ids.is_empty()
                && canonical_ids.iter().all(|canonical_id| {
                    superseding_canonical_ids
                        .get(canonical_id)
                        .is_some_and(|superseding_ids| {
                            superseding_ids
                                .iter()
                                .any(|newer_id| present_canonical_ids.contains(newer_id))
                        })
                })
        }) {
            continue;
        }
        if chunk_canonicals.is_some_and(|canonical_ids| {
            canonical_ids
                .iter()
                .any(|canonical_id| selected_canonical_ids.contains(canonical_id))
        }) {
            continue;
        }

        let normalized = normalize_text(hit.content());
        if normalized.is_empty() {
            continue;
        }

        if normalized_selected
            .iter()
            .any(|existing| are_near_duplicates(existing, &normalized))
        {
            continue;
        }

        if let Some(canonical_ids) = chunk_canonicals {
            selected_canonical_ids.extend(canonical_ids.iter().cloned());
        }
        normalized_selected.push(normalized);
        selected.push(hit);
    }

    selected
}

async fn load_chunk_canonical_ids(
    fact_store: &FactStore,
    agent_id: &str,
    hits: &[MemoryHit],
) -> Result<HashMap<String, Vec<String>>> {
    let chunk_ids = hits
        .iter()
        .filter_map(|hit| hit.chunk_id().map(ToOwned::to_owned))
        .collect::<Vec<_>>();
    if chunk_ids.is_empty() {
        return Ok(HashMap::new());
    }

    MemoryLineageStore::new(fact_store.db())
        .get_canonical_ids_for_sources(agent_id, "chunk", &chunk_ids)
        .await
}

async fn load_fact_canonical_ids(
    fact_store: &FactStore,
    agent_id: &str,
    hits: &[MemoryHit],
) -> Result<HashMap<String, Vec<String>>> {
    let fact_ids = hits
        .iter()
        .filter_map(|hit| hit.fact_id().map(ToOwned::to_owned))
        .collect::<Vec<_>>();
    if fact_ids.is_empty() {
        return Ok(HashMap::new());
    }

    MemoryLineageStore::new(fact_store.db())
        .get_canonical_ids_for_sources(agent_id, "fact", &fact_ids)
        .await
}

async fn load_superseding_chunk_canonical_ids(
    fact_store: &FactStore,
    agent_id: &str,
    chunk_canonical_ids: &HashMap<String, Vec<String>>,
) -> Result<HashMap<String, Vec<String>>> {
    let canonical_ids = chunk_canonical_ids
        .values()
        .flat_map(|ids| ids.iter().cloned())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    if canonical_ids.is_empty() {
        return Ok(HashMap::new());
    }

    MemoryLineageStore::new(fact_store.db())
        .get_superseding_canonical_ids(agent_id, &canonical_ids)
        .await
}

pub fn filter_duplicate_chunks_against_facts(
    chunks: Vec<SearchResult>,
    facts: &[Fact],
) -> Vec<SearchResult> {
    let normalized_facts = facts
        .iter()
        .map(|fact| normalize_text(&fact.content))
        .filter(|content| !content.is_empty())
        .collect::<Vec<_>>();

    chunks
        .into_iter()
        .filter(|chunk| {
            let normalized_chunk = normalize_text(&chunk.text);
            !normalized_facts
                .iter()
                .any(|fact| are_near_duplicates(fact, &normalized_chunk))
        })
        .collect()
}

pub fn rerank_chunks_by_source(results: &mut [SearchResult], bias: MemoryRoutingBias) {
    for result in &mut *results {
        result.score *= source_weight(classify_chunk_source(&result.source, &result.path), bias);
    }
    results.sort_by(|a, b| b.score.total_cmp(&a.score));
}

pub fn classify_chunk_source(source: &str, path: &str) -> MemorySourceKind {
    match source {
        "long_term" => MemorySourceKind::LongTerm,
        "daily" => MemorySourceKind::Daily,
        "session" => MemorySourceKind::Session,
        _ if path == "MEMORY.md" => MemorySourceKind::LongTerm,
        _ if path.starts_with("memory/") => MemorySourceKind::Daily,
        _ if path.starts_with("sessions/") => MemorySourceKind::Session,
        _ => MemorySourceKind::Other,
    }
}

pub fn source_label(kind: MemorySourceKind) -> &'static str {
    match kind {
        MemorySourceKind::Fact => "fact",
        MemorySourceKind::LongTerm => "long_term",
        MemorySourceKind::Daily => "daily",
        MemorySourceKind::Session => "session",
        MemorySourceKind::Other => "other",
    }
}

fn score_facts(
    facts: &[Fact],
    query: &str,
    min_score: f64,
    bias: MemoryRoutingBias,
) -> Vec<MemoryFactHit> {
    let normalized_query = normalize_text(query);
    if normalized_query.is_empty() {
        return Vec::new();
    }
    let query_tokens = tokenize(&normalized_query);
    if query_tokens.is_empty() {
        return Vec::new();
    }

    let mut hits = facts
        .iter()
        .filter_map(|fact| {
            let normalized_content = normalize_text(&fact.content);
            if normalized_content.is_empty() {
                return None;
            }

            let content_tokens = tokenize(&normalized_content);
            let overlap = token_overlap_ratio(&query_tokens, &content_tokens);
            let contains_query = normalized_content.contains(&normalized_query);
            let fact_type_match = fact
                .fact_type
                .split_whitespace()
                .map(|token| token.to_ascii_lowercase())
                .any(|token| query_tokens.contains(&token));

            if overlap == 0.0 && !contains_query && !fact_type_match {
                return None;
            }

            let mut score = overlap * 0.7;
            if contains_query {
                score += 0.35;
            }
            if fact_type_match {
                score += 0.1;
            }
            score += fact.importance.clamp(0.0, 1.0) * 0.15;
            score += fact.confidence.clamp(0.0, 1.0) * 0.2;
            score += fact.salience.clamp(0, 100) as f64 * 0.002;
            // Emotionally significant facts get a ranking boost
            score += fact.affect_intensity.clamp(0.0, 1.0) * 0.05;
            score *= source_weight(MemorySourceKind::Fact, bias);

            if score < min_score {
                return None;
            }

            Some(MemoryFactHit {
                fact: fact.clone(),
                score,
            })
        })
        .collect::<Vec<_>>();

    hits.sort_by(|a, b| b.score.total_cmp(&a.score));
    hits
}

fn source_weight(kind: MemorySourceKind, bias: MemoryRoutingBias) -> f64 {
    match bias {
        MemoryRoutingBias::Neutral => match kind {
            MemorySourceKind::Fact => 1.25,
            MemorySourceKind::LongTerm => 1.1,
            MemorySourceKind::Daily => 1.0,
            MemorySourceKind::Session => 0.85,
            MemorySourceKind::Other => 1.0,
        },
        MemoryRoutingBias::LongTerm => match kind {
            MemorySourceKind::Fact => 1.35,
            MemorySourceKind::LongTerm => 1.2,
            MemorySourceKind::Daily => 0.95,
            MemorySourceKind::Session => 0.75,
            MemorySourceKind::Other => 1.0,
        },
        MemoryRoutingBias::ShortTerm => match kind {
            MemorySourceKind::Fact => 0.95,
            MemorySourceKind::LongTerm => 0.9,
            MemorySourceKind::Daily => 1.18,
            MemorySourceKind::Session => 1.05,
            MemorySourceKind::Other => 1.0,
        },
    }
}

pub fn infer_memory_routing_bias(hits: &[MemoryHit]) -> MemoryRoutingBias {
    if hits.is_empty() {
        return MemoryRoutingBias::Neutral;
    }

    let mut long_signal = 0.0_f64;
    let mut short_signal = 0.0_f64;

    for (idx, hit) in hits.iter().take(6).enumerate() {
        let rank_weight = 1.0 / ((idx + 1) as f64);
        let contribution = hit.score().max(0.0) * rank_weight;
        match hit.source_kind() {
            MemorySourceKind::Fact | MemorySourceKind::LongTerm => {
                long_signal += contribution;
                short_signal -= contribution * 0.45;
            }
            MemorySourceKind::Daily | MemorySourceKind::Session => {
                short_signal += contribution;
                long_signal -= contribution * 0.35;
            }
            MemorySourceKind::Other => {}
        }
    }

    if long_signal > short_signal + 0.18 {
        MemoryRoutingBias::LongTerm
    } else if short_signal > long_signal + 0.18 {
        MemoryRoutingBias::ShortTerm
    } else {
        MemoryRoutingBias::Neutral
    }
}

pub(crate) fn normalize_text(text: &str) -> String {
    text.chars()
        .map(|ch| {
            if ch.is_alphanumeric() || ch.is_whitespace() {
                ch.to_ascii_lowercase()
            } else {
                ' '
            }
        })
        .collect::<String>()
}

pub(crate) fn are_near_duplicates(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }

    if a.len() >= 24 && (a.contains(b) || b.contains(a)) {
        return true;
    }

    let tokens_a = tokenize(a);
    let tokens_b = tokenize(b);
    if tokens_a.is_empty() || tokens_b.is_empty() {
        return false;
    }

    token_overlap_ratio(&tokens_a, &tokens_b) >= 0.8
}

fn is_cjk(ch: char) -> bool {
    matches!(ch,
        '\u{4E00}'..='\u{9FFF}'   // CJK Unified Ideographs
        | '\u{3400}'..='\u{4DBF}' // CJK Unified Ideographs Extension A
        | '\u{3000}'..='\u{303F}' // CJK Symbols and Punctuation
        | '\u{3040}'..='\u{309F}' // Hiragana
        | '\u{30A0}'..='\u{30FF}' // Katakana
        | '\u{AC00}'..='\u{D7AF}' // Hangul Syllables
    )
}

fn tokenize(text: &str) -> HashSet<String> {
    let mut tokens = HashSet::new();

    // Whitespace-based tokens for alphabetic/mixed text
    for word in text.split_whitespace() {
        if word.len() > 1 {
            tokens.insert(word.to_string());
        }
    }

    // CJK: extract unigrams and bigrams from runs of CJK characters
    let cjk_chars: Vec<char> = text.chars().filter(|ch| is_cjk(*ch)).collect();
    for ch in &cjk_chars {
        tokens.insert(ch.to_string());
    }
    for pair in cjk_chars.windows(2) {
        tokens.insert(format!("{}{}", pair[0], pair[1]));
    }

    tokens
}

pub(crate) fn find_matching_fact<'a>(facts: &'a [Fact], content: &str) -> Option<&'a Fact> {
    facts
        .iter()
        .find(|fact| is_matching_memory_content(&fact.content, content))
}

pub(crate) fn is_matching_memory_content(existing: &str, candidate: &str) -> bool {
    let normalized_existing = normalize_text(existing);
    let normalized_candidate = normalize_text(candidate);
    !normalized_existing.is_empty()
        && !normalized_candidate.is_empty()
        && are_near_duplicates(&normalized_existing, &normalized_candidate)
}

fn token_overlap_ratio(query_tokens: &HashSet<String>, content_tokens: &HashSet<String>) -> f64 {
    if query_tokens.is_empty() || content_tokens.is_empty() {
        return 0.0;
    }

    let overlap = query_tokens.intersection(content_tokens).count() as f64;
    overlap / query_tokens.len() as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_result(path: &str, source: &str, score: f64) -> SearchResult {
        SearchResult {
            chunk_id: format!("{path}:0-1:test"),
            path: path.to_string(),
            source: source.to_string(),
            start_line: 0,
            end_line: 1,
            snippet: "demo".to_string(),
            text: "demo".to_string(),
            score,
            score_breakdown: None,
        }
    }

    #[test]
    fn rerank_prefers_long_term_over_session_when_scores_are_close() {
        let mut results = vec![
            make_result("sessions/demo", "session", 0.82),
            make_result("MEMORY.md", "long_term", 0.8),
        ];

        rerank_chunks_by_source(&mut results, MemoryRoutingBias::Neutral);

        assert_eq!(results[0].path, "MEMORY.md");
        assert!(results[0].score > results[1].score);
    }

    #[test]
    fn score_facts_matches_query_content() {
        let fact = Fact {
            id: "fact-1".to_string(),
            agent_id: "agent".to_string(),
            content: "User prefers Chinese replies".to_string(),
            fact_type: "preference".to_string(),
            importance: 0.8,
            confidence: 1.0,
            status: "active".to_string(),
            occurred_at: None,
            recorded_at: "2026-03-29T00:00:00Z".to_string(),
            source_type: "agent_write".to_string(),
            source_session: None,
            access_count: 0,
            last_accessed: None,
            superseded_by: None,
            salience: 50,
            supersede_reason: None,
            affect: "neutral".to_string(),
            affect_intensity: 0.0,
            created_at: "2026-03-29T00:00:00Z".to_string(),
            updated_at: "2026-03-29T00:00:00Z".to_string(),
        };

        let hits = score_facts(&[fact], "Chinese replies", 0.2, MemoryRoutingBias::Neutral);

        assert_eq!(hits.len(), 1);
        assert!(hits[0].score > 0.2);
    }

    #[test]
    fn score_facts_includes_salience_signal_in_score() {
        let fact = Fact {
            id: "fact-salience".to_string(),
            agent_id: "agent".to_string(),
            content: "User likes ramen".to_string(),
            fact_type: "preference".to_string(),
            importance: 0.5,
            confidence: 0.5,
            status: "active".to_string(),
            occurred_at: None,
            recorded_at: "2026-03-29T00:00:00Z".to_string(),
            source_type: "agent_write".to_string(),
            source_session: None,
            access_count: 0,
            last_accessed: None,
            superseded_by: None,
            salience: 100,
            supersede_reason: None,
            affect: "neutral".to_string(),
            affect_intensity: 0.0,
            created_at: "2026-03-29T00:00:00Z".to_string(),
            updated_at: "2026-03-29T00:00:00Z".to_string(),
        };

        let hits = score_facts(&[fact], "likes ramen", 0.0, MemoryRoutingBias::Neutral);

        assert_eq!(hits.len(), 1);
        let expected = 1.78125;
        assert!((hits[0].score - expected).abs() < 1e-6);
    }

    #[test]
    fn score_facts_prefers_high_confidence_and_salience_with_same_relevance() {
        let low_signal = Fact {
            id: "fact-low-signal".to_string(),
            agent_id: "agent".to_string(),
            content: "User likes ramen".to_string(),
            fact_type: "preference".to_string(),
            importance: 0.5,
            confidence: 0.2,
            status: "active".to_string(),
            occurred_at: None,
            recorded_at: "2026-03-29T00:00:00Z".to_string(),
            source_type: "agent_write".to_string(),
            source_session: None,
            access_count: 0,
            last_accessed: None,
            superseded_by: None,
            salience: 10,
            supersede_reason: None,
            affect: "neutral".to_string(),
            affect_intensity: 0.0,
            created_at: "2026-03-29T00:00:00Z".to_string(),
            updated_at: "2026-03-29T00:00:00Z".to_string(),
        };

        let high_signal = Fact {
            id: "fact-high-signal".to_string(),
            confidence: 1.0,
            salience: 100,
            ..low_signal.clone()
        };

        let hits = score_facts(
            &[low_signal, high_signal],
            "likes ramen",
            0.0,
            MemoryRoutingBias::Neutral,
        );

        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].fact.id, "fact-high-signal");

        let score_delta = hits[0].score - hits[1].score;
        let expected_delta = 0.425;
        assert!((score_delta - expected_delta).abs() < 1e-6);
    }

    #[test]
    fn score_facts_ranks_high_salience_above_low_salience_when_relevance_is_equal() {
        let low_salience = Fact {
            id: "fact-low".to_string(),
            agent_id: "agent".to_string(),
            content: "User likes ramen".to_string(),
            fact_type: "preference".to_string(),
            importance: 0.6,
            confidence: 0.9,
            status: "active".to_string(),
            occurred_at: None,
            recorded_at: "2026-03-29T00:00:00Z".to_string(),
            source_type: "agent_write".to_string(),
            source_session: None,
            access_count: 0,
            last_accessed: None,
            superseded_by: None,
            salience: 10,
            supersede_reason: None,
            affect: "neutral".to_string(),
            affect_intensity: 0.0,
            created_at: "2026-03-29T00:00:00Z".to_string(),
            updated_at: "2026-03-29T00:00:00Z".to_string(),
        };
        let high_salience = Fact {
            id: "fact-high".to_string(),
            salience: 95,
            ..low_salience.clone()
        };

        let hits = score_facts(
            &[low_salience, high_salience],
            "likes ramen",
            0.0,
            MemoryRoutingBias::Neutral,
        );

        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].fact.id, "fact-high");
        assert!(hits[0].score > hits[1].score);
    }

    #[test]
    fn infer_memory_routing_bias_prefers_long_term_when_top_hits_are_durable() {
        let hits = vec![
            MemoryHit::Chunk(Box::new(make_result("MEMORY.md", "long_term", 0.92))),
            MemoryHit::Fact(Box::new(MemoryFactHit {
                fact: Fact {
                    id: "fact-1".to_string(),
                    agent_id: "agent".to_string(),
                    content: "User prefers Chinese replies".to_string(),
                    fact_type: "preference".to_string(),
                    importance: 0.8,
                    confidence: 1.0,
                    status: "active".to_string(),
                    occurred_at: None,
                    recorded_at: "2026-03-29T00:00:00Z".to_string(),
                    source_type: "explicit_user_memory".to_string(),
                    source_session: None,
                    access_count: 0,
                    last_accessed: None,
                    superseded_by: None,
                    salience: 50,
                    supersede_reason: None,
                    affect: "neutral".to_string(),
                    affect_intensity: 0.0,
                    created_at: "2026-03-29T00:00:00Z".to_string(),
                    updated_at: "2026-03-29T00:00:00Z".to_string(),
                },
                score: 0.78,
            })),
        ];

        assert_eq!(
            infer_memory_routing_bias(&hits),
            MemoryRoutingBias::LongTerm
        );
    }

    #[test]
    fn infer_memory_routing_bias_prefers_short_term_when_recent_hits_dominate() {
        let hits = vec![
            MemoryHit::Chunk(Box::new(make_result("memory/2026-03-30.md", "daily", 0.91))),
            MemoryHit::Chunk(Box::new(make_result(
                "sessions/demo#turn:1-2",
                "session",
                0.77,
            ))),
            MemoryHit::Chunk(Box::new(make_result("MEMORY.md", "long_term", 0.42))),
        ];

        assert_eq!(
            infer_memory_routing_bias(&hits),
            MemoryRoutingBias::ShortTerm
        );
    }

    #[test]
    fn dedup_memory_hits_prefers_first_hit_for_near_duplicates() {
        let fact = Fact {
            id: "fact-1".to_string(),
            agent_id: "agent".to_string(),
            content: "User prefers Chinese replies for all future answers".to_string(),
            fact_type: "preference".to_string(),
            importance: 0.8,
            confidence: 1.0,
            status: "active".to_string(),
            occurred_at: None,
            recorded_at: "2026-03-29T00:00:00Z".to_string(),
            source_type: "agent_write".to_string(),
            source_session: None,
            access_count: 0,
            last_accessed: None,
            superseded_by: None,
            salience: 50,
            supersede_reason: None,
            affect: "neutral".to_string(),
            affect_intensity: 0.0,
            created_at: "2026-03-29T00:00:00Z".to_string(),
            updated_at: "2026-03-29T00:00:00Z".to_string(),
        };
        let chunk = SearchResult {
            chunk_id: "chunk-1".to_string(),
            path: "MEMORY.md".to_string(),
            source: "long_term".to_string(),
            start_line: 1,
            end_line: 3,
            snippet: "User prefers Chinese replies for all future answers".to_string(),
            text: "User prefers Chinese replies for all future answers".to_string(),
            score: 0.9,
            score_breakdown: None,
        };

        let hits = dedup_memory_hits(vec![
            MemoryHit::Fact(Box::new(MemoryFactHit { fact, score: 1.0 })),
            MemoryHit::Chunk(Box::new(chunk)),
        ]);

        assert_eq!(hits.len(), 1);
        assert!(matches!(hits[0], MemoryHit::Fact(_)));
    }

    #[test]
    fn dedup_memory_hits_drops_later_chunk_with_same_canonical() {
        let hits = vec![
            MemoryHit::Chunk(Box::new(SearchResult {
                chunk_id: "chunk-memory".to_string(),
                path: "MEMORY.md".to_string(),
                source: "long_term".to_string(),
                start_line: 1,
                end_line: 2,
                snippet: "ActionBook 采用 CDP 抽取文章结构".to_string(),
                text: "ActionBook 采用 CDP 抽取文章结构".to_string(),
                score: 1.1,
                score_breakdown: None,
            })),
            MemoryHit::Chunk(Box::new(SearchResult {
                chunk_id: "chunk-daily".to_string(),
                path: "memory/2026-03-29.md".to_string(),
                source: "daily".to_string(),
                start_line: 4,
                end_line: 5,
                snippet: "今天讨论了 ActionBook 的 CDP 文章抽取方案".to_string(),
                text: "今天讨论了 ActionBook 的 CDP 文章抽取方案".to_string(),
                score: 0.95,
                score_breakdown: None,
            })),
        ];
        let chunk_canonical_ids = HashMap::from([
            (
                "chunk-memory".to_string(),
                vec!["canon-1".to_string(), "canon-shared".to_string()],
            ),
            ("chunk-daily".to_string(), vec!["canon-shared".to_string()]),
        ]);

        let deduped = dedup_memory_hits_with_chunk_canonicals(hits, &chunk_canonical_ids);

        assert_eq!(deduped.len(), 1);
        assert_eq!(deduped[0].chunk_id(), Some("chunk-memory"));
    }

    #[test]
    fn dedup_memory_hits_drops_chunk_when_all_its_canonicals_are_superseded() {
        let hits = vec![
            MemoryHit::Chunk(Box::new(SearchResult {
                chunk_id: "chunk-old".to_string(),
                path: "memory/2026-03-28.md".to_string(),
                source: "daily".to_string(),
                start_line: 1,
                end_line: 2,
                snippet: "Use incremental patch consolidation for memory".to_string(),
                text: "Use incremental patch consolidation for memory".to_string(),
                score: 1.2,
                score_breakdown: None,
            })),
            MemoryHit::Chunk(Box::new(SearchResult {
                chunk_id: "chunk-new".to_string(),
                path: "MEMORY.md".to_string(),
                source: "long_term".to_string(),
                start_line: 1,
                end_line: 2,
                snippet: "Use section-based consolidation for memory".to_string(),
                text: "Use section-based consolidation for memory".to_string(),
                score: 0.9,
                score_breakdown: None,
            })),
        ];
        let chunk_canonical_ids = HashMap::from([
            ("chunk-old".to_string(), vec!["canon-old".to_string()]),
            ("chunk-new".to_string(), vec!["canon-new".to_string()]),
        ]);
        let superseding_canonical_ids =
            HashMap::from([("canon-old".to_string(), vec!["canon-new".to_string()])]);

        let deduped = dedup_memory_hits_with_chunk_canonicals_and_supersedes(
            hits,
            &HashMap::new(),
            &chunk_canonical_ids,
            &superseding_canonical_ids,
        );

        assert_eq!(deduped.len(), 1);
        assert_eq!(deduped[0].chunk_id(), Some("chunk-new"));
    }

    #[test]
    fn dedup_memory_hits_drops_chunk_when_fact_shares_memory_canonical() {
        let fact = Fact {
            id: "fact-1".to_string(),
            agent_id: "agent".to_string(),
            content: "Use section-based consolidation for memory".to_string(),
            fact_type: "decision".to_string(),
            importance: 0.9,
            confidence: 1.0,
            status: "active".to_string(),
            occurred_at: None,
            recorded_at: "2026-03-29T00:00:00Z".to_string(),
            source_type: "agent_write".to_string(),
            source_session: None,
            access_count: 0,
            last_accessed: None,
            superseded_by: None,
            salience: 50,
            supersede_reason: None,
            affect: "neutral".to_string(),
            affect_intensity: 0.0,
            created_at: "2026-03-29T00:00:00Z".to_string(),
            updated_at: "2026-03-29T00:00:00Z".to_string(),
        };
        let hits = vec![
            MemoryHit::Fact(Box::new(MemoryFactHit { fact, score: 1.1 })),
            MemoryHit::Chunk(Box::new(SearchResult {
                chunk_id: "chunk-memory".to_string(),
                path: "MEMORY.md".to_string(),
                source: "long_term".to_string(),
                start_line: 1,
                end_line: 2,
                snippet: "The project uses section-based consolidation for memory".to_string(),
                text: "The project uses section-based consolidation for memory".to_string(),
                score: 1.05,
                score_breakdown: None,
            })),
        ];
        let fact_canonical_ids = HashMap::from([(
            "fact-1".to_string(),
            vec!["canon-fact".to_string(), "canon-memory".to_string()],
        )]);
        let chunk_canonical_ids =
            HashMap::from([("chunk-memory".to_string(), vec!["canon-memory".to_string()])]);

        let deduped = dedup_memory_hits_with_chunk_canonicals_and_supersedes(
            hits,
            &fact_canonical_ids,
            &chunk_canonical_ids,
            &HashMap::new(),
        );

        assert_eq!(deduped.len(), 1);
        assert!(matches!(deduped[0], MemoryHit::Fact(_)));
    }

    #[test]
    fn tokenize_handles_cjk_text() {
        let tokens = tokenize("用户喜欢吃拉面");
        // Should contain CJK unigrams
        assert!(tokens.contains("用"));
        assert!(tokens.contains("喜"));
        assert!(tokens.contains("面"));
        // Should contain CJK bigrams
        assert!(tokens.contains("喜欢"));
        assert!(tokens.contains("拉面"));
    }

    #[test]
    fn cjk_fact_scores_above_min_threshold() {
        let fact = Fact {
            id: "test".to_string(),
            agent_id: "agent-1".to_string(),
            content: "用户喜欢吃拉面".to_string(),
            fact_type: "preference".to_string(),
            importance: 0.5,
            confidence: 1.0,
            status: "active".to_string(),
            occurred_at: None,
            recorded_at: "2026-01-01T00:00:00Z".to_string(),
            source_type: "agent_write".to_string(),
            source_session: None,
            access_count: 0,
            last_accessed: None,
            superseded_by: None,
            salience: 50,
            supersede_reason: None,
            affect: "neutral".to_string(),
            affect_intensity: 0.0,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
        };
        let hits = score_facts(&[fact], "我喜欢吃什么面", 0.25, MemoryRoutingBias::Neutral);
        assert!(
            !hits.is_empty(),
            "CJK query '我喜欢吃什么面' should match fact '用户喜欢吃拉面'"
        );
        assert!(
            hits[0].score >= 0.25,
            "score {} should be >= 0.25",
            hits[0].score
        );
    }

    #[test]
    fn filter_facts_by_time_range_uses_occurred_at() {
        let in_range = Fact {
            id: "fact-in".to_string(),
            agent_id: "agent-1".to_string(),
            content: "In range fact".to_string(),
            fact_type: "event".to_string(),
            importance: 0.5,
            confidence: 1.0,
            status: "active".to_string(),
            occurred_at: Some("2026-03-15T10:00:00Z".to_string()),
            recorded_at: "2026-03-15T10:00:00Z".to_string(),
            source_type: "agent_write".to_string(),
            source_session: None,
            access_count: 0,
            last_accessed: None,
            superseded_by: None,
            salience: 50,
            supersede_reason: None,
            affect: "neutral".to_string(),
            affect_intensity: 0.0,
            created_at: "2026-03-15T10:00:00Z".to_string(),
            updated_at: "2026-03-15T10:00:00Z".to_string(),
        };
        let out_of_range = Fact {
            id: "fact-out".to_string(),
            occurred_at: Some("2026-04-01T00:00:00Z".to_string()),
            ..in_range.clone()
        };

        let filtered = filter_facts_by_time_range(
            &[in_range, out_of_range],
            Some(&TimeRange {
                from: Some("2026-03".to_string()),
                to: Some("2026-03".to_string()),
            }),
        );

        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].id, "fact-in");
    }
}
