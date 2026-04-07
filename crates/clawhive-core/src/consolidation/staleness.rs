use anyhow::Result;
use chrono::{DateTime, Utc};

use super::prompts::STALE_SECTION_CONFIRM_SYSTEM_PROMPT;
use super::text_utils::reference_half_life_days;
use super::{HippocampusConsolidator, StaleSectionCandidate};
use crate::memory_document::{MemoryDocument, MEMORY_SECTION_ORDER};

impl HippocampusConsolidator {
    pub(super) async fn evaluate_memory_staleness(&self) -> Result<Vec<StaleSectionCandidate>> {
        let current_memory = self.file_store.read_long_term().await?;
        let doc = MemoryDocument::parse(&current_memory);
        let mut candidates = Vec::new();

        for section in MEMORY_SECTION_ORDER {
            let content = doc.section_content(section);
            if content.trim().is_empty() {
                continue;
            }

            let (total_access_count, max_last_accessed) =
                self.query_memory_section_access_stats(&content).await?;

            let Some(last_accessed) = max_last_accessed else {
                continue;
            };
            if total_access_count <= 0 {
                continue;
            }

            let Ok(last_accessed_at) = DateTime::parse_from_rfc3339(&last_accessed) else {
                tracing::warn!(section, value = %last_accessed, "Invalid last_accessed timestamp");
                continue;
            };

            let days_since_accessed = (Utc::now() - last_accessed_at.with_timezone(&Utc))
                .num_seconds()
                .max(0) as f64
                / 86_400.0;
            let staleness_score = days_since_accessed / reference_half_life_days(section);

            if staleness_score > 3.0 {
                candidates.push(StaleSectionCandidate {
                    section: section.to_string(),
                    content,
                    staleness_score,
                    days_since_accessed,
                });
            }
        }

        Ok(candidates)
    }

    async fn query_memory_section_access_stats(
        &self,
        section_text: &str,
    ) -> Result<(i64, Option<String>)> {
        if let Some(index) = &self.search_index {
            return index
                .query_section_access_stats("MEMORY.md", section_text)
                .await;
        }

        Ok((0, None))
    }

    pub(super) async fn prune_stale_sections(
        &self,
        candidates: &[StaleSectionCandidate],
    ) -> Result<usize> {
        if candidates.is_empty() {
            return Ok(0);
        }

        let mut doc = MemoryDocument::parse(&self.file_store.read_long_term().await?);
        let archived_at = Utc::now().format("%Y-%m-%d").to_string();
        let mut pruned = 0usize;

        for candidate in candidates {
            let response = match self
                .request_consolidation(
                    STALE_SECTION_CONFIRM_SYSTEM_PROMPT,
                    format!(
                        "Section: {}\nStaleness score: {:.2}\nDays since last accessed: {:.1}\n\nContent:\n{}\n\nIs this section stale and safe to archive? Answer STALE if yes, KEEP if no.",
                        candidate.section,
                        candidate.staleness_score,
                        candidate.days_since_accessed,
                        candidate.content,
                    ),
                )
                .await
            {
                Ok(r) => r,
                Err(error) => {
                    tracing::warn!(
                        section = %candidate.section,
                        %error,
                        "LLM confirmation failed for stale section; skipping candidate"
                    );
                    continue;
                }
            };

            let decision = response.text.trim().to_ascii_uppercase();
            if !(decision.contains("STALE") || decision.contains("YES")) {
                tracing::info!(section = %candidate.section, decision = %response.text, "Stale candidate rejected by LLM");
                continue;
            }

            self.file_store
                .append_archived_section(&candidate.section, &candidate.content, &archived_at)
                .await?;
            doc.remove_section(&candidate.section);
            pruned += 1;
            tracing::info!(
                section = %candidate.section,
                staleness_score = candidate.staleness_score,
                "Archived stale MEMORY.md section"
            );
        }

        if pruned > 0 {
            self.file_store.write_long_term(&doc.render()).await?;
        }

        Ok(pruned)
    }
}
