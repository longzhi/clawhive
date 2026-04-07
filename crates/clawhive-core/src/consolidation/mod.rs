use std::sync::Arc;

use clawhive_memory::embedding::EmbeddingProvider;
use clawhive_memory::file_store::MemoryFileStore;
use clawhive_memory::search_index::SearchIndex;
use clawhive_memory::session::SessionReader;
use clawhive_memory::MemoryStore;

use super::router::LlmRouter;

mod prompts;

mod text_utils;
use text_utils::default_importance;
pub(crate) use text_utils::jaccard_similarity;
pub(crate) use text_utils::normalized_word_set;

mod patch;
pub use patch::apply_patch;
pub use patch::parse_patch;

mod matching;

mod fact_reconciliation;

mod staleness;

mod lineage;

mod consolidate;

mod scheduler;
pub use scheduler::{ConsolidationScheduler, GcReport};

#[cfg(test)]
mod test_helpers;

#[derive(Debug, Clone, serde::Deserialize)]
struct PromotionCandidate {
    content: String,
    target_kind: String,
    target_section: Option<String>,
    source_date: Option<String>,
    #[serde(default = "default_importance")]
    importance: f64,
    #[serde(default)]
    duplicate_key: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AddInstruction {
    pub section: String,
    pub content: String,
}

#[derive(Debug, Clone)]
pub struct UpdateInstruction {
    pub old: String,
    pub new: String,
}

#[derive(Debug, Clone)]
pub struct MemoryPatch {
    pub adds: Vec<AddInstruction>,
    pub updates: Vec<UpdateInstruction>,
    pub keep: bool,
}

#[derive(Debug, Clone)]
struct StaleSectionCandidate {
    section: String,
    content: String,
    staleness_score: f64,
    days_since_accessed: f64,
}

pub struct HippocampusConsolidator {
    agent_id: String,
    pub(crate) file_store: MemoryFileStore,
    router: Arc<LlmRouter>,
    model_primary: String,
    model_fallbacks: Vec<String>,
    model_compaction: Option<String>,
    lookback_days: usize,
    search_index: Option<SearchIndex>,
    embedding_provider: Option<Arc<dyn EmbeddingProvider>>,
    reindex_file_store: Option<MemoryFileStore>,
    reindex_session_reader: Option<SessionReader>,
    memory_store: Option<Arc<MemoryStore>>,
    embedding_cache_ttl_days: u64,
    session_idle_minutes: i64,
}

#[derive(Debug)]
pub struct ConsolidationReport {
    pub daily_files_read: usize,
    pub memory_updated: bool,
    pub reindexed: bool,
    pub facts_extracted: usize,
    pub summary: String,
}

impl HippocampusConsolidator {
    pub fn new(
        agent_id: String,
        file_store: MemoryFileStore,
        router: Arc<LlmRouter>,
        model_primary: String,
        model_fallbacks: Vec<String>,
    ) -> Self {
        Self {
            agent_id,
            file_store,
            router,
            model_primary,
            model_fallbacks,
            model_compaction: None,
            lookback_days: 7,
            search_index: None,
            embedding_provider: None,
            reindex_file_store: None,
            reindex_session_reader: None,
            memory_store: None,
            embedding_cache_ttl_days: 90,
            session_idle_minutes: 30,
        }
    }

    pub fn with_lookback_days(mut self, days: usize) -> Self {
        self.lookback_days = days;
        self
    }

    pub fn with_search_index(mut self, index: SearchIndex) -> Self {
        self.search_index = Some(index);
        self
    }

    pub fn with_embedding_provider(mut self, provider: Arc<dyn EmbeddingProvider>) -> Self {
        self.embedding_provider = Some(provider);
        self
    }

    pub fn with_file_store_for_reindex(mut self, file_store: MemoryFileStore) -> Self {
        self.reindex_file_store = Some(file_store);
        self
    }

    pub fn with_memory_store(mut self, store: Arc<MemoryStore>) -> Self {
        self.memory_store = Some(store);
        self
    }

    pub fn with_embedding_cache_ttl_days(mut self, ttl_days: u64) -> Self {
        self.embedding_cache_ttl_days = ttl_days;
        self
    }

    pub fn with_session_idle_minutes(mut self, idle_minutes: i64) -> Self {
        self.session_idle_minutes = idle_minutes.max(1);
        self
    }

    pub fn with_model_compaction(mut self, model: String) -> Self {
        self.model_compaction = Some(model);
        self
    }

    pub fn with_session_reader_for_reindex(mut self, reader: SessionReader) -> Self {
        self.reindex_session_reader = Some(reader);
        self
    }

    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    pub(crate) fn search_index(&self) -> Option<&SearchIndex> {
        self.search_index.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use anyhow::Result;

    use super::test_helpers::*;
    use super::*;

    #[test]
    fn consolidation_report_default_fields() {
        let report = ConsolidationReport {
            daily_files_read: 0,
            memory_updated: false,
            reindexed: false,
            facts_extracted: 0,
            summary: "none".to_string(),
        };

        assert_eq!(report.daily_files_read, 0);
        assert!(!report.memory_updated);
        assert!(!report.reindexed);
        assert_eq!(report.facts_extracted, 0);
        assert_eq!(report.summary, "none");
    }

    #[test]
    fn hippocampus_new_default_lookback() -> Result<()> {
        let (_dir, file_store) = build_file_store()?;
        let consolidator = HippocampusConsolidator::new(
            "agent-1".to_string(),
            file_store,
            build_router(),
            "sonnet".to_string(),
            vec![],
        );

        assert_eq!(consolidator.lookback_days, 7);
        Ok(())
    }

    #[test]
    fn hippocampus_with_lookback_days() -> Result<()> {
        let (_dir, file_store) = build_file_store()?;
        let consolidator = HippocampusConsolidator::new(
            "agent-1".to_string(),
            file_store,
            build_router(),
            "sonnet".to_string(),
            vec![],
        )
        .with_lookback_days(30);

        assert_eq!(consolidator.lookback_days, 30);
        Ok(())
    }
}
