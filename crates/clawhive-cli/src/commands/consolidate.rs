use std::path::Path;
use std::sync::Arc;

use anyhow::Result;

use clawhive_core::*;

use crate::runtime::bootstrap::{bootstrap, build_embedding_provider, build_router_from_config};

pub(crate) async fn run(root: &Path, agent_id_override: Option<&str>) -> Result<()> {
    let (_bus, memory, _gateway, config, _schedule_manager, _wait_manager, _approval_registry) =
        bootstrap(root, None).await?;

    let consolidation_agent_id = agent_id_override
        .map(str::to_owned)
        .unwrap_or_else(|| config.routing.default_agent_id.clone());
    let agent_config = config
        .agents
        .iter()
        .find(|agent| agent.agent_id == consolidation_agent_id)
        .ok_or_else(|| anyhow::anyhow!("agent '{}' not found in config", consolidation_agent_id))?;
    let consolidation_workspace = Workspace::resolve(
        root,
        &agent_config.agent_id,
        agent_config.workspace.as_deref(),
    );
    let workspace_dir = consolidation_workspace.root().to_path_buf();
    let file_store = clawhive_memory::file_store::MemoryFileStore::new(&workspace_dir);
    let session_reader = clawhive_memory::session::SessionReader::new(&workspace_dir);
    let consolidation_search_index = clawhive_memory::search_index::SearchIndex::new_with_config(
        memory.db(),
        &consolidation_agent_id,
        clawhive_memory::search_index::SearchConfig {
            vector_weight: config.main.memory_search.vector_weight,
            bm25_weight: config.main.memory_search.bm25_weight,
            decay_half_life_days: config.main.memory_search.decay_half_life_days,
            mmr_lambda: config.main.memory_search.mmr_lambda,
            access_boost_factor: config.main.memory_search.access_boost_factor,
            hot_days: config.main.memory_search.temperature.hot_days,
            warm_days: config.main.memory_search.temperature.warm_days,
            cold_filter: config.main.memory_search.temperature.cold_filter,
            access_protect_count: config.main.memory_search.temperature.access_protect_count,
            max_results: config.main.memory_search.max_results,
            min_score: config.main.memory_search.min_score,
            embedding_cache_ttl_days: config.main.memory_search.embedding_cache_ttl_days,
        },
    );
    let consolidation_embedding_provider = build_embedding_provider(&config).await;
    let mut consolidator_builder = HippocampusConsolidator::new(
        consolidation_agent_id,
        file_store.clone(),
        Arc::new(build_router_from_config(&config).await),
        agent_config.model_policy.primary.clone(),
        agent_config.model_policy.fallbacks.clone(),
    )
    .with_search_index(consolidation_search_index)
    .with_embedding_provider(consolidation_embedding_provider)
    .with_file_store_for_reindex(file_store)
    .with_session_reader_for_reindex(session_reader)
    .with_memory_store(Arc::clone(&memory))
    .with_session_idle_minutes(
        agent_config
            .memory_policy
            .as_ref()
            .and_then(|policy| policy.idle_minutes)
            .unwrap_or(30) as i64,
    )
    .with_embedding_cache_ttl_days(config.main.memory_search.embedding_cache_ttl_days);
    if let Some(compaction_model) = agent_config.model_policy.compaction_model.clone() {
        consolidator_builder = consolidator_builder.with_model_compaction(compaction_model);
    }
    let consolidator = Arc::new(consolidator_builder);

    let scheduler = ConsolidationScheduler::new(
        vec![consolidator],
        config.main.consolidation_schedule.clone(),
        config.main.archive_retention_days,
    );
    println!("Running hippocampus consolidation...");
    let results = scheduler.run_once().await;
    for (agent_id, result) in results {
        match result {
            Ok(report) => {
                println!("Consolidation complete for {agent_id}:");
                println!("  Daily files read: {}", report.daily_files_read);
                println!("  Memory updated: {}", report.memory_updated);
                println!("  Reindexed: {}", report.reindexed);
                println!("  Facts extracted: {}", report.facts_extracted);
                println!("  Summary: {}", report.summary);
            }
            Err(e) => {
                eprintln!("Consolidation failed for {agent_id}: {e}");
            }
        }
    }
    Ok(())
}
