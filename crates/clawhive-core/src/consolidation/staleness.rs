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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use anyhow::Result;
    use chrono::Utc;
    use clawhive_memory::search_index::SearchIndex;
    use clawhive_memory::store::MemoryStore;

    use crate::consolidation::test_helpers::*;
    use crate::consolidation::{HippocampusConsolidator, StaleSectionCandidate};

    #[tokio::test]
    async fn evaluate_memory_staleness_marks_only_sections_above_threshold() -> Result<()> {
        let (_dir, file_store) = build_file_store()?;
        file_store
            .write_long_term(
                "# MEMORY.md\n\n## 长期项目主线\n\n- stale-unit12-mainline\n\n## 持续性背景脉络\n\n- fresh-unit12-context\n\n## 关键历史决策\n\n- untouched-unit12-decision\n",
            )
            .await?;

        let memory_store = Arc::new(MemoryStore::open_in_memory()?);
        let search_index = SearchIndex::new(memory_store.db(), "agent-1");
        search_index
            .index_file(
                "MEMORY.md",
                &file_store.read_long_term().await?,
                "long_term",
                &StubEmbeddingProvider,
            )
            .await?;

        {
            let db = memory_store.db();
            let conn = db.lock().expect("lock");
            let stale_ts = (Utc::now() - chrono::Duration::days(95)).to_rfc3339();
            let fresh_ts = (Utc::now() - chrono::Duration::days(10)).to_rfc3339();
            conn.execute(
                "UPDATE chunks SET access_count = 2, last_accessed = ?1 WHERE agent_id = 'agent-1' AND path = 'MEMORY.md' AND text LIKE '%stale-unit12-mainline%'",
                [stale_ts],
            )?;
            conn.execute(
                "UPDATE chunks SET access_count = 5, last_accessed = ?1 WHERE agent_id = 'agent-1' AND path = 'MEMORY.md' AND text LIKE '%fresh-unit12-context%'",
                [fresh_ts],
            )?;
        }

        let consolidator = HippocampusConsolidator::new(
            "agent-1".to_string(),
            file_store,
            build_router(),
            "sonnet".to_string(),
            vec![],
        )
        .with_search_index(search_index)
        .with_memory_store(Arc::clone(&memory_store));

        let candidates = consolidator.evaluate_memory_staleness().await?;
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].section, "长期项目主线");
        assert!(candidates[0].staleness_score > 3.0);
        Ok(())
    }

    #[tokio::test]
    async fn prune_stale_sections_moves_section_to_archived_when_llm_confirms() -> Result<()> {
        let (_dir, file_store) = build_file_store()?;
        file_store
            .write_long_term(
                "# MEMORY.md\n\n## 长期项目主线\n\n- stale-candidate-unit12\n\n## 持续性背景脉络\n\n- keep-context\n\n## 关键历史决策\n\n- keep-decision\n",
            )
            .await?;

        let router = build_router_with_provider(SequenceProvider::new(vec!["STALE".to_string()]));
        let consolidator = HippocampusConsolidator::new(
            "agent-1".to_string(),
            file_store.clone(),
            router,
            "sonnet".to_string(),
            vec![],
        );

        let candidates = vec![StaleSectionCandidate {
            section: "长期项目主线".to_string(),
            content: "- stale-candidate-unit12".to_string(),
            staleness_score: 3.2,
            days_since_accessed: 96.0,
        }];

        let pruned = consolidator.prune_stale_sections(&candidates).await?;
        assert_eq!(pruned, 1);

        let memory = file_store.read_long_term().await?;
        let archived = file_store.read_archived_long_term().await?;
        assert!(!memory.contains("stale-candidate-unit12"));
        assert!(archived.contains("## 长期项目主线 (archived "));
        assert!(archived.contains("stale-candidate-unit12"));
        Ok(())
    }

    #[tokio::test]
    async fn prune_stale_sections_keeps_memory_when_llm_rejects_candidate() -> Result<()> {
        let (_dir, file_store) = build_file_store()?;
        file_store
            .write_long_term(
                "# MEMORY.md\n\n## 长期项目主线\n\n- keep-unit12-candidate\n\n## 持续性背景脉络\n\n- keep-context\n\n## 关键历史决策\n\n- keep-decision\n",
            )
            .await?;

        let router = build_router_with_provider(SequenceProvider::new(vec!["KEEP".to_string()]));
        let consolidator = HippocampusConsolidator::new(
            "agent-1".to_string(),
            file_store.clone(),
            router,
            "sonnet".to_string(),
            vec![],
        );

        let candidates = vec![StaleSectionCandidate {
            section: "长期项目主线".to_string(),
            content: "- keep-unit12-candidate".to_string(),
            staleness_score: 4.1,
            days_since_accessed: 123.0,
        }];

        let pruned = consolidator.prune_stale_sections(&candidates).await?;
        assert_eq!(pruned, 0);

        let memory = file_store.read_long_term().await?;
        let archived = file_store.read_archived_long_term().await?;
        assert!(memory.contains("keep-unit12-candidate"));
        assert!(!archived.contains("keep-unit12-candidate"));
        Ok(())
    }
}
