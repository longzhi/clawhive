use std::sync::Arc;

use anyhow::Result;
use nanocrab_memory::embedding::EmbeddingProvider;
use nanocrab_memory::file_store::MemoryFileStore;
use nanocrab_memory::search_index::SearchIndex;
use nanocrab_provider::LlmMessage;

use super::router::LlmRouter;

const CONSOLIDATION_SYSTEM_PROMPT: &str = r#"You are a memory consolidation system. You maintain a personal knowledge base (MEMORY.md)
by integrating new daily observations.

Rules:
- Preserve existing important knowledge that is still valid
- Add new stable facts, user preferences, and behavioral patterns from daily notes
- Remove or update information that has been contradicted by newer observations
- Use clear Markdown formatting with headers for organization
- Be concise - only keep information that is useful for future conversations
- Output the COMPLETE updated MEMORY.md content (not a diff)"#;

pub struct HippocampusConsolidator {
    file_store: MemoryFileStore,
    router: Arc<LlmRouter>,
    model_primary: String,
    model_fallbacks: Vec<String>,
    lookback_days: usize,
    search_index: Option<SearchIndex>,
    embedding_provider: Option<Arc<dyn EmbeddingProvider>>,
    reindex_file_store: Option<MemoryFileStore>,
}

#[derive(Debug)]
pub struct ConsolidationReport {
    pub daily_files_read: usize,
    pub memory_updated: bool,
    pub reindexed: bool,
    pub summary: String,
}

impl HippocampusConsolidator {
    pub fn new(
        file_store: MemoryFileStore,
        router: Arc<LlmRouter>,
        model_primary: String,
        model_fallbacks: Vec<String>,
    ) -> Self {
        Self {
            file_store,
            router,
            model_primary,
            model_fallbacks,
            lookback_days: 7,
            search_index: None,
            embedding_provider: None,
            reindex_file_store: None,
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

    pub async fn consolidate(&self) -> Result<ConsolidationReport> {
        let current_memory = self.file_store.read_long_term().await?;
        let recent_daily = self
            .file_store
            .read_recent_daily(self.lookback_days)
            .await?;

        if recent_daily.is_empty() {
            return Ok(ConsolidationReport {
                daily_files_read: 0,
                memory_updated: false,
                reindexed: false,
                summary: "No daily files found in lookback window; skipped consolidation."
                    .to_string(),
            });
        }

        let mut daily_sections = String::new();
        for (date, content) in &recent_daily {
            daily_sections.push_str(&format!("### {}\n{}\n\n", date.format("%Y-%m-%d"), content));
        }

        let user_prompt = format!(
            "## Current MEMORY.md\n{}\n\n## Recent Daily Observations\n{}\nPlease synthesize the daily observations into an updated MEMORY.md.\nOutput ONLY the new MEMORY.md content, no explanations.",
            current_memory, daily_sections
        );

        let response = self
            .router
            .chat(
                &self.model_primary,
                &self.model_fallbacks,
                Some(CONSOLIDATION_SYSTEM_PROMPT.to_string()),
                vec![LlmMessage::user(user_prompt)],
                4096,
            )
            .await?;

        let updated_memory = strip_markdown_fence(&response.text);
        self.file_store.write_long_term(&updated_memory).await?;

        let reindexed = if let (Some(index), Some(provider), Some(fs)) = (
            &self.search_index,
            &self.embedding_provider,
            &self.reindex_file_store,
        ) {
            match index.index_all(fs, provider.as_ref()).await {
                Ok(count) => {
                    tracing::info!("Post-consolidation reindex: {count} chunks indexed");
                    true
                }
                Err(e) => {
                    tracing::warn!("Post-consolidation reindex failed: {e}");
                    false
                }
            }
        } else {
            false
        };

        Ok(ConsolidationReport {
            daily_files_read: recent_daily.len(),
            memory_updated: true,
            reindexed,
            summary: format!(
                "Consolidated {} daily files into MEMORY.md.",
                recent_daily.len()
            ),
        })
    }
}

fn strip_markdown_fence(text: &str) -> String {
    let trimmed = text.trim();
    let without_prefix = trimmed
        .strip_prefix("```markdown")
        .or_else(|| trimmed.strip_prefix("```md"))
        .or_else(|| trimmed.strip_prefix("```"))
        .unwrap_or(trimmed)
        .trim_start();
    without_prefix
        .strip_suffix("```")
        .unwrap_or(without_prefix)
        .trim_end()
        .to_string()
}

pub struct ConsolidationScheduler {
    consolidator: Arc<HippocampusConsolidator>,
    interval_hours: u64,
}

impl ConsolidationScheduler {
    pub fn new(consolidator: Arc<HippocampusConsolidator>, interval_hours: u64) -> Self {
        Self {
            consolidator,
            interval_hours,
        }
    }

    pub fn start(self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(tokio::time::Duration::from_secs(self.interval_hours * 3600));
            interval.tick().await;
            loop {
                interval.tick().await;
                tracing::info!("Running scheduled hippocampus consolidation...");
                match self.consolidator.consolidate().await {
                    Ok(report) => {
                        tracing::info!(
                            "Consolidation complete: daily_files_read={}, memory_updated={}, summary={}",
                            report.daily_files_read,
                            report.memory_updated,
                            report.summary
                        );
                    }
                    Err(err) => {
                        tracing::error!("Consolidation failed: {err}");
                    }
                }
            }
        })
    }

    pub async fn run_once(&self) -> Result<ConsolidationReport> {
        self.consolidator.consolidate().await
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use anyhow::Result;
    use async_trait::async_trait;
    use nanocrab_memory::embedding::EmbeddingProvider;
    use nanocrab_memory::file_store::MemoryFileStore;
    use nanocrab_provider::{ProviderRegistry, StubProvider};
    use tempfile::TempDir;

    use super::{ConsolidationReport, ConsolidationScheduler, HippocampusConsolidator};
    use crate::router::LlmRouter;

    fn build_router() -> Arc<LlmRouter> {
        let mut registry = ProviderRegistry::new();
        registry.register("anthropic", Arc::new(StubProvider));
        let aliases = HashMap::from([(
            "sonnet".to_string(),
            "anthropic/claude-sonnet-4-5".to_string(),
        )]);
        Arc::new(LlmRouter::new(registry, aliases, vec![]))
    }

    fn build_file_store() -> Result<(TempDir, MemoryFileStore)> {
        let dir = TempDir::new()?;
        let store = MemoryFileStore::new(dir.path());
        Ok((dir, store))
    }

    #[test]
    fn consolidation_report_default_fields() {
        let report = ConsolidationReport {
            daily_files_read: 0,
            memory_updated: false,
            reindexed: false,
            summary: "none".to_string(),
        };

        assert_eq!(report.daily_files_read, 0);
        assert!(!report.memory_updated);
        assert!(!report.reindexed);
        assert_eq!(report.summary, "none");
    }

    #[test]
    fn hippocampus_new_default_lookback() -> Result<()> {
        let (_dir, file_store) = build_file_store()?;
        let consolidator =
            HippocampusConsolidator::new(file_store, build_router(), "sonnet".to_string(), vec![]);

        assert_eq!(consolidator.lookback_days, 7);
        Ok(())
    }

    #[test]
    fn hippocampus_with_lookback_days() -> Result<()> {
        let (_dir, file_store) = build_file_store()?;
        let consolidator =
            HippocampusConsolidator::new(file_store, build_router(), "sonnet".to_string(), vec![])
                .with_lookback_days(30);

        assert_eq!(consolidator.lookback_days, 30);
        Ok(())
    }

    #[test]
    fn consolidation_scheduler_new() -> Result<()> {
        let (_dir, file_store) = build_file_store()?;
        let consolidator = Arc::new(HippocampusConsolidator::new(
            file_store,
            build_router(),
            "sonnet".to_string(),
            vec![],
        ));

        let scheduler = ConsolidationScheduler::new(Arc::clone(&consolidator), 24);
        assert_eq!(scheduler.interval_hours, 24);
        Ok(())
    }

    #[tokio::test]
    async fn consolidation_no_daily_files_returns_early() -> Result<()> {
        let (_dir, file_store) = build_file_store()?;
        file_store.write_long_term("# Memory\n\nExisting").await?;

        let consolidator =
            HippocampusConsolidator::new(file_store, build_router(), "sonnet".to_string(), vec![]);

        let report = consolidator.consolidate().await?;
        assert_eq!(report.daily_files_read, 0);
        assert!(!report.memory_updated);
        assert!(report.summary.contains("No daily files found"));
        Ok(())
    }

    #[tokio::test]
    async fn consolidation_triggers_reindex_after_write() -> Result<()> {
        use chrono::Local;
        use nanocrab_memory::search_index::SearchIndex;
        use nanocrab_memory::store::MemoryStore;

        // Create temp dir and file store
        let (_dir, file_store) = build_file_store()?;

        // Write MEMORY.md
        file_store
            .write_long_term("# Existing Memory\n\nSome knowledge.")
            .await?;

        // Write today's daily file
        let today = Local::now().date_naive();
        file_store
            .write_daily(today, "## Today's Observations\n\nLearned something new.")
            .await?;

        // Create in-memory MemoryStore and SearchIndex
        let memory_store = MemoryStore::open_in_memory()?;
        let search_index = SearchIndex::new(memory_store.db());

        // Create a stub embedding provider
        let embedding_provider = Arc::new(StubEmbeddingProvider);

        // Create consolidator with re-indexing enabled
        let consolidator = HippocampusConsolidator::new(
            file_store.clone(),
            build_router(),
            "sonnet".to_string(),
            vec![],
        )
        .with_search_index(search_index.clone())
        .with_embedding_provider(embedding_provider)
        .with_file_store_for_reindex(file_store);

        // Run consolidation
        let report = consolidator.consolidate().await?;

        // Verify consolidation succeeded
        assert!(report.memory_updated);
        assert_eq!(report.daily_files_read, 1);

        // Verify re-indexing happened
        assert!(report.reindexed);

        Ok(())
    }

    struct StubEmbeddingProvider;

    #[async_trait]
    impl EmbeddingProvider for StubEmbeddingProvider {
        async fn embed(
            &self,
            texts: &[String],
        ) -> anyhow::Result<nanocrab_memory::embedding::EmbeddingResult> {
            let embeddings = texts.iter().map(|_| vec![0.1; 384]).collect();
            Ok(nanocrab_memory::embedding::EmbeddingResult {
                embeddings,
                model: "stub".to_string(),
                dimensions: 384,
            })
        }

        fn model_id(&self) -> &str {
            "stub"
        }

        fn dimensions(&self) -> usize {
            384
        }
    }
}
