use std::path::Path;
use std::sync::Arc;

use anyhow::Result;

use clawhive_core::*;

use crate::runtime::bootstrap::{bootstrap, build_embedding_provider, build_router_from_config};

pub(crate) async fn run(root: &Path) -> Result<()> {
    let (_bus, memory, _gateway, config, _schedule_manager, _wait_manager, _approval_registry) =
        bootstrap(root, None).await?;

    let consolidation_agent_id = config.routing.default_agent_id.clone();
    let consolidation_workspace = config
        .agents
        .iter()
        .find(|agent| agent.agent_id == consolidation_agent_id)
        .map(|agent| Workspace::resolve(root, &agent.agent_id, agent.workspace.as_deref()))
        .unwrap_or_else(|| Workspace::resolve(root, &consolidation_agent_id, None));
    let workspace_dir = consolidation_workspace.root().to_path_buf();
    let file_store = clawhive_memory::file_store::MemoryFileStore::new(&workspace_dir);
    let session_reader = clawhive_memory::session::SessionReader::new(&workspace_dir);
    let consolidation_search_index = clawhive_memory::search_index::SearchIndex::new(memory.db());
    let consolidation_embedding_provider = build_embedding_provider(&config).await;
    let consolidator = Arc::new(
        HippocampusConsolidator::new(
            consolidation_agent_id,
            file_store.clone(),
            Arc::new(build_router_from_config(&config).await),
            "sonnet".to_string(),
            vec!["haiku".to_string()],
        )
        .with_search_index(consolidation_search_index)
        .with_embedding_provider(consolidation_embedding_provider)
        .with_file_store_for_reindex(file_store)
        .with_session_reader_for_reindex(session_reader)
        .with_memory_store(Arc::clone(&memory)),
    );

    let scheduler = ConsolidationScheduler::new(
        vec![consolidator],
        config.main.consolidation_interval_hours,
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
