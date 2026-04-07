use std::collections::HashSet;

use clawhive_memory::fact_store::Fact;

use super::text_utils::{jaccard_similarity, normalized_word_set};
use super::PromotionCandidate;
use crate::memory_document::MEMORY_SECTION_ORDER;

pub(super) fn dedup_memory_candidates(
    candidates: Vec<PromotionCandidate>,
) -> Vec<PromotionCandidate> {
    let mut kept = Vec::new();
    let mut seen = std::collections::BTreeSet::new();

    for candidate in candidates {
        if candidate.target_kind != "memory" || candidate.importance < 0.3 {
            continue;
        }

        let Some(section) = candidate.target_section.as_deref() else {
            continue;
        };
        if !MEMORY_SECTION_ORDER.contains(&section) {
            continue;
        }

        let content = candidate.content.trim();
        if content.is_empty() {
            continue;
        }

        let key = candidate
            .duplicate_key
            .as_deref()
            .map(|value| value.trim().to_lowercase())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| content.to_lowercase());

        if !seen.insert((section.to_string(), key)) {
            continue;
        }

        kept.push(PromotionCandidate {
            content: content.to_string(),
            ..candidate
        });
    }

    kept
}

pub(super) fn best_matching_candidate_for_item<'a>(
    section: &str,
    item: &str,
    candidates: &'a [PromotionCandidate],
) -> Option<&'a PromotionCandidate> {
    let item_normalized = normalize_lineage_text(item);
    let item_words = tokenize_lineage_text(item);
    let section_candidates = candidates
        .iter()
        .filter(|candidate| candidate.target_section.as_deref() == Some(section))
        .collect::<Vec<_>>();

    if let Some((candidate, _)) = section_candidates
        .iter()
        .filter_map(|candidate| {
            let candidate_normalized = normalize_lineage_text(&candidate.content);
            let candidate_words = tokenize_lineage_text(&candidate.content);
            let similarity = if item_normalized.contains(&candidate_normalized)
                || candidate_normalized.contains(&item_normalized)
            {
                1.0
            } else {
                jaccard_similarity(&item_words, &candidate_words)
            };
            (similarity >= 0.55).then_some((candidate, similarity))
        })
        .max_by(|(_, left), (_, right)| left.total_cmp(right))
    {
        return Some(*candidate);
    }

    let keyed_candidates = section_candidates
        .into_iter()
        .filter(|candidate| {
            candidate
                .duplicate_key
                .as_deref()
                .map(str::trim)
                .is_some_and(|value| !value.is_empty())
        })
        .collect::<Vec<_>>();

    if keyed_candidates.len() == 1 {
        return keyed_candidates.into_iter().next();
    }

    None
}

pub(super) fn should_link_supersedes(old_item: &str, new_item: &str) -> bool {
    let old_normalized = normalize_lineage_text(old_item);
    let new_normalized = normalize_lineage_text(new_item);
    if old_normalized.is_empty() || new_normalized.is_empty() || old_normalized == new_normalized {
        return false;
    }
    let old_words = tokenize_lineage_text(old_item);
    let new_words = tokenize_lineage_text(new_item);
    jaccard_similarity(&old_words, &new_words) >= 0.5
}

pub(super) fn best_matching_memory_item_for_fact<'a>(
    fact: &Fact,
    memory_items: &'a [String],
) -> Option<&'a str> {
    let fact_normalized = normalize_lineage_text(&fact.content);
    let fact_words = tokenize_lineage_text(&fact.content);
    memory_items
        .iter()
        .filter_map(|item| {
            let item_normalized = normalize_lineage_text(item);
            let item_words = tokenize_lineage_text(item);
            let similarity = if item_normalized.contains(&fact_normalized)
                || fact_normalized.contains(&item_normalized)
            {
                1.0
            } else {
                jaccard_similarity(&fact_words, &item_words)
            };
            (similarity >= 0.6).then_some((item.as_str(), similarity))
        })
        .max_by(|(_, left), (_, right)| left.total_cmp(right))
        .map(|(item, _)| item)
}

pub(super) fn tokenize_lineage_text(input: &str) -> HashSet<String> {
    input
        .split(|c: char| !c.is_alphanumeric() && !('\u{4E00}'..='\u{9FFF}').contains(&c))
        .filter(|part| !part.is_empty())
        .map(|part| part.to_lowercase())
        .collect()
}

pub(super) fn normalize_lineage_text(input: &str) -> String {
    input
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

pub(super) fn fact_conflict_step_b_passes(new_fact: &Fact, existing: &Fact) -> bool {
    if new_fact.fact_type != existing.fact_type {
        return false;
    }

    let new_tokens = normalized_word_set(&new_fact.content);
    let existing_tokens = normalized_word_set(&existing.content);
    jaccard_similarity(&new_tokens, &existing_tokens) > 0.6
}
