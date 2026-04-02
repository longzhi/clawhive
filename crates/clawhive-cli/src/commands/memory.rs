use std::collections::HashSet;
use std::path::Path;

use anyhow::Result;
use chrono::NaiveDate;
use clap::Subcommand;
use clawhive_core::{MemoryDocument, MEMORY_SECTION_ORDER};
use clawhive_memory::dirty_sources::{
    DirtySourceStore, DIRTY_KIND_DAILY_FILE, DIRTY_KIND_FACT, DIRTY_KIND_MEMORY_FILE,
    DIRTY_KIND_SESSION,
};
use clawhive_memory::fact_store::FactStore;
use clawhive_memory::file_audit::{audit_memory_file, cleanup_memory_file, MemoryFileKind};
use clawhive_memory::file_store::MemoryFileStore;
use clawhive_memory::memory_lineage::MemoryLineageStore;
use clawhive_memory::search_index::{SearchConfig, SearchIndex};
use clawhive_memory::SessionReader;

use crate::runtime::bootstrap::{bootstrap, build_embedding_provider};

#[derive(Subcommand)]
pub enum MemoryCommands {
    #[command(about = "Show memory index statistics")]
    Stats {
        #[arg(help = "Agent ID")]
        agent_id: String,
    },
    #[command(about = "Show memory trace audit log for an agent")]
    Audit {
        #[arg(help = "Agent ID to audit")]
        agent_id: String,
        #[arg(long, short = 'n', default_value = "20", help = "Number of entries")]
        limit: usize,
        #[arg(long, help = "Only show entries on/after YYYY-MM-DD")]
        since: Option<String>,
        #[arg(long, help = "Expand details for section merges")]
        detail: bool,
    },
    #[command(about = "Audit MEMORY.md and daily files for prompt leakage and low-value residue")]
    AuditMemoryFiles {
        #[arg(help = "Agent ID to audit")]
        agent_id: String,
        #[arg(
            long,
            default_value = "30",
            help = "How many recent daily files to include (0 = all)"
        )]
        days: usize,
    },
    #[command(
        about = "Dry-run cleanup for MEMORY.md and daily files based on high-confidence rules"
    )]
    CleanupMemoryFiles {
        #[arg(help = "Agent ID to clean up")]
        agent_id: String,
        #[arg(
            long,
            default_value = "30",
            help = "How many recent daily files to include (0 = all)"
        )]
        days: usize,
        #[arg(
            long,
            help = "Apply cleaned content instead of printing dry-run output"
        )]
        apply: bool,
    },
    #[command(about = "Rebuild session chunks for an agent using the current turn/topic rules")]
    MigrateSessionIndex {
        #[arg(help = "Agent ID to migrate")]
        agent_id: String,
    },
    #[command(
        about = "Backfill canonical/lineage records for existing facts, MEMORY.md, and daily files"
    )]
    MigrateLineage {
        #[arg(help = "Agent ID to migrate")]
        agent_id: String,
        #[arg(
            long,
            default_value = "30",
            help = "How many recent daily files to include (0 = all)"
        )]
        days: usize,
    },
    #[command(about = "Rebuild search index from memory files")]
    RebuildIndex {
        #[arg(help = "Agent ID to rebuild")]
        agent_id: String,
    },
    #[command(about = "Export all memory for an agent (facts, MEMORY.md, daily files)")]
    Export {
        #[arg(help = "Agent ID to export")]
        agent_id: String,
        #[arg(long, help = "Export format: json or markdown (default: json)")]
        format: Option<String>,
    },
}

pub async fn run(cmd: MemoryCommands, root: &Path) -> Result<()> {
    let (_bus, memory, _gateway, config, _schedule_manager, _wait_manager, _approval_registry) =
        bootstrap(root, None).await?;

    match cmd {
        MemoryCommands::Stats { agent_id } => {
            let stats = memory.stats_for_agent(&agent_id).await?;

            println!("Memory Statistics ({agent_id}):");
            println!("  chunks:            {}", stats.chunk_count);
            println!("  facts:             {}", stats.fact_count);
            println!("  pending_dirty:     {}", stats.pending_dirty_sources);
            println!("  embedding_cache:   {}", stats.embedding_cache_count);
            println!("  trace_entries:     {}", stats.trace_count);

            Ok(())
        }
        MemoryCommands::Audit {
            agent_id,
            limit,
            since,
            detail,
        } => {
            let traces = memory
                .list_traces(&agent_id, limit, since.as_deref())
                .await?;

            if traces.is_empty() {
                println!("No trace entries found for agent '{agent_id}'.");
                return Ok(());
            }

            println!("timestamp | operation | details_summary");
            println!("--------- | --------- | ---------------");
            for trace in &traces {
                let summary = summarize_trace_details(&trace.details);
                println!("{} | {} | {}", trace.timestamp, trace.operation, summary);
                if detail && trace.operation == "section_merge" {
                    if let Ok(details_json) =
                        serde_json::from_str::<serde_json::Value>(&trace.details)
                    {
                        if let Some(rendered) = format_section_merge_detail(&details_json) {
                            for line in rendered.lines() {
                                println!("  {line}");
                            }
                        }
                    }
                }
            }

            println!("\nShowing {} entries (newest first).", traces.len());
            Ok(())
        }
        MemoryCommands::AuditMemoryFiles { agent_id, days } => {
            let workspace_dir = root.join("workspaces").join(&agent_id);
            let file_store = MemoryFileStore::new(&workspace_dir);
            let files = collect_memory_files(&file_store, days).await?;

            let mut total_findings = 0usize;
            for file in files {
                let findings = audit_memory_file(&file.path, &file.content, file.kind);
                if findings.is_empty() {
                    continue;
                }

                total_findings += findings.len();
                println!("{}\n  findings: {}", file.path, findings.len());
                for finding in findings {
                    let line = finding
                        .line
                        .map(|line| format!(" line {line}"))
                        .unwrap_or_default();
                    println!("  - [{:?}] {}{}", finding.severity, finding.message, line);
                }
                println!();
            }

            if total_findings == 0 {
                println!("No memory file issues found for agent '{agent_id}'.");
            } else {
                println!("Total findings: {total_findings}");
            }

            Ok(())
        }
        MemoryCommands::CleanupMemoryFiles {
            agent_id,
            days,
            apply,
        } => {
            let workspace_dir = root.join("workspaces").join(&agent_id);
            let file_store = MemoryFileStore::new(&workspace_dir);
            let files = collect_memory_files(&file_store, days).await?;
            let mut dirty_sources = Vec::new();

            let mut changed = 0usize;
            for file in files {
                let cleaned = cleanup_memory_file(&file.content, file.kind);
                if cleaned.content == file.content {
                    continue;
                }

                changed += 1;
                println!(
                    "{}\n  removed_prompt_leakage_lines={}\n  removed_empty_headings={}\n  removed_duplicate_bullets={}\n  removed_trivial_chat_lines={}",
                    file.path,
                    cleaned.stats.removed_prompt_leakage_lines,
                    cleaned.stats.removed_empty_headings,
                    cleaned.stats.removed_duplicate_bullets,
                    cleaned.stats.removed_trivial_chat_lines,
                );

                if apply {
                    match file.kind {
                        MemoryFileKind::LongTerm => {
                            file_store.write_long_term(&cleaned.content).await?;
                            dirty_sources.push((
                                DIRTY_KIND_MEMORY_FILE.to_string(),
                                "MEMORY.md".to_string(),
                            ));
                        }
                        MemoryFileKind::Daily => {
                            if let Some(date) = file.date {
                                file_store.write_daily(date, &cleaned.content).await?;
                                dirty_sources.push((
                                    DIRTY_KIND_DAILY_FILE.to_string(),
                                    format!("memory/{}.md", date.format("%Y-%m-%d")),
                                ));
                            }
                        }
                    }
                    println!("  applied\n");
                } else {
                    println!("  dry-run cleaned content:");
                    println!("```md\n{}```\n", cleaned.content);
                }
            }

            if changed == 0 {
                println!("No cleanup changes suggested for agent '{agent_id}'.");
            } else if !apply {
                println!("Dry-run complete. {changed} file(s) would change.");
            } else {
                let dirty_store = DirtySourceStore::new(memory.db());
                for (source_kind, source_ref) in &dirty_sources {
                    dirty_store
                        .enqueue(&agent_id, source_kind, source_ref, "cleanup_memory_files")
                        .await?;
                }

                if !dirty_sources.is_empty() {
                    let session_reader = SessionReader::new(&workspace_dir);
                    let search_index = SearchIndex::new_with_config(
                        memory.db(),
                        &agent_id,
                        SearchConfig {
                            vector_weight: config.main.memory_search.vector_weight,
                            bm25_weight: config.main.memory_search.bm25_weight,
                            decay_half_life_days: config.main.memory_search.decay_half_life_days,
                            mmr_lambda: config.main.memory_search.mmr_lambda,
                            access_boost_factor: config.main.memory_search.access_boost_factor,
                            hot_days: config.main.memory_search.temperature.hot_days,
                            warm_days: config.main.memory_search.temperature.warm_days,
                            cold_filter: config.main.memory_search.temperature.cold_filter,
                            access_protect_count: config
                                .main
                                .memory_search
                                .temperature
                                .access_protect_count,
                            max_results: config.main.memory_search.max_results,
                            min_score: config.main.memory_search.min_score,
                            embedding_cache_ttl_days: config
                                .main
                                .memory_search
                                .embedding_cache_ttl_days,
                        },
                    );
                    let embedding_provider = build_embedding_provider(&config).await;
                    search_index
                        .process_dirty_sources(
                            &dirty_store,
                            &agent_id,
                            &file_store,
                            &session_reader,
                            embedding_provider.as_ref(),
                            dirty_sources.len(),
                        )
                        .await?;
                }
                println!("Cleanup applied to {changed} file(s).");
            }

            Ok(())
        }
        MemoryCommands::MigrateSessionIndex { agent_id } => {
            let workspace_dir = root.join("workspaces").join(&agent_id);
            let file_store = MemoryFileStore::new(&workspace_dir);
            let session_reader = SessionReader::new(&workspace_dir);
            let search_index = SearchIndex::new_with_config(
                memory.db(),
                &agent_id,
                SearchConfig {
                    vector_weight: config.main.memory_search.vector_weight,
                    bm25_weight: config.main.memory_search.bm25_weight,
                    decay_half_life_days: config.main.memory_search.decay_half_life_days,
                    mmr_lambda: config.main.memory_search.mmr_lambda,
                    access_boost_factor: config.main.memory_search.access_boost_factor,
                    hot_days: config.main.memory_search.temperature.hot_days,
                    warm_days: config.main.memory_search.temperature.warm_days,
                    cold_filter: config.main.memory_search.temperature.cold_filter,
                    access_protect_count: config
                        .main
                        .memory_search
                        .temperature
                        .access_protect_count,
                    max_results: config.main.memory_search.max_results,
                    min_score: config.main.memory_search.min_score,
                    embedding_cache_ttl_days: config.main.memory_search.embedding_cache_ttl_days,
                },
            );
            let embedding_provider = build_embedding_provider(&config).await;

            println!("Rebuilding session index for agent '{agent_id}'...");
            let count = search_index
                .index_sessions(&session_reader, embedding_provider.as_ref())
                .await?;
            println!("Done. Indexed {count} session chunks.");
            let _ = file_store;
            Ok(())
        }
        MemoryCommands::MigrateLineage { agent_id, days } => {
            let workspace_dir = root.join("workspaces").join(&agent_id);
            let file_store = MemoryFileStore::new(&workspace_dir);
            let fact_store = FactStore::new(memory.db());
            let lineage_store = MemoryLineageStore::new(memory.db());
            let files = collect_memory_files(&file_store, days).await?;
            let active_facts = fact_store.get_active_facts(&agent_id).await?;

            let mut fact_count = 0usize;
            for fact in &active_facts {
                lineage_store.link_fact(fact).await?;
                fact_count += 1;
            }

            let mut memory_item_count = 0usize;
            let mut daily_item_count = 0usize;
            let mut daily_entries = Vec::new();

            for file in files
                .iter()
                .filter(|file| matches!(file.kind, MemoryFileKind::Daily))
            {
                let Some(date) = file.date else {
                    continue;
                };
                for item in extract_daily_items(&file.content) {
                    let canonical = lineage_store
                        .ensure_canonical(&agent_id, "daily", &item)
                        .await?;
                    lineage_store
                        .attach_source(
                            &agent_id,
                            &canonical.canonical_id,
                            "daily_section",
                            &format!(
                                "memory/{}.md#{}",
                                date.format("%Y-%m-%d"),
                                canonical.canonical_id
                            ),
                            "summary",
                        )
                        .await?;
                    lineage_store
                        .attach_matching_chunks(
                            &agent_id,
                            &canonical.canonical_id,
                            &format!("memory/{}.md", date.format("%Y-%m-%d")),
                            &item,
                            "summary",
                        )
                        .await?;
                    daily_entries.push(DailyCanonicalEntry {
                        content: item,
                        canonical_id: canonical.canonical_id.clone(),
                    });
                    daily_item_count += 1;
                }
            }

            if let Some(long_term) = files
                .iter()
                .find(|file| matches!(file.kind, MemoryFileKind::LongTerm))
            {
                let doc = MemoryDocument::parse(&long_term.content);
                for section in MEMORY_SECTION_ORDER {
                    for item in doc.section_items(section) {
                        let canonical = lineage_store
                            .ensure_canonical(&agent_id, "memory", &item)
                            .await?;
                        lineage_store
                            .attach_source(
                                &agent_id,
                                &canonical.canonical_id,
                                "memory_section",
                                &format!("MEMORY.md#{}#{}", section, canonical.canonical_id),
                                "promoted",
                            )
                            .await?;
                        lineage_store
                            .attach_matching_chunks_in_section(
                                &agent_id,
                                &canonical.canonical_id,
                                "MEMORY.md",
                                section,
                                &item,
                                "promoted",
                            )
                            .await?;

                        if let Some(daily_entry) =
                            find_best_canonical_match(&item, &daily_entries, 0.55)
                        {
                            lineage_store
                                .attach_source(
                                    &agent_id,
                                    &canonical.canonical_id,
                                    "canonical",
                                    &daily_entry.canonical_id,
                                    "supersedes",
                                )
                                .await?;
                        }

                        for fact in active_facts
                            .iter()
                            .filter(|fact| memory_entry_matches_fact(&item, &fact.content))
                        {
                            lineage_store
                                .attach_source(
                                    &agent_id,
                                    &canonical.canonical_id,
                                    "fact",
                                    &fact.id,
                                    "equivalent",
                                )
                                .await?;
                        }

                        memory_item_count += 1;
                    }
                }
            }

            println!(
                "Lineage migration complete for '{agent_id}': facts={fact_count}, memory_items={memory_item_count}, daily_items={daily_item_count}"
            );
            Ok(())
        }
        MemoryCommands::RebuildIndex { agent_id } => {
            let workspace_dir = root.join("workspaces").join(&agent_id);
            let file_store = MemoryFileStore::new(&workspace_dir);
            let session_reader = SessionReader::new(&workspace_dir);
            let search_index = SearchIndex::new_with_config(
                memory.db(),
                &agent_id,
                SearchConfig {
                    vector_weight: config.main.memory_search.vector_weight,
                    bm25_weight: config.main.memory_search.bm25_weight,
                    decay_half_life_days: config.main.memory_search.decay_half_life_days,
                    mmr_lambda: config.main.memory_search.mmr_lambda,
                    access_boost_factor: config.main.memory_search.access_boost_factor,
                    hot_days: config.main.memory_search.temperature.hot_days,
                    warm_days: config.main.memory_search.temperature.warm_days,
                    cold_filter: config.main.memory_search.temperature.cold_filter,
                    access_protect_count: config
                        .main
                        .memory_search
                        .temperature
                        .access_protect_count,
                    max_results: config.main.memory_search.max_results,
                    min_score: config.main.memory_search.min_score,
                    embedding_cache_ttl_days: config.main.memory_search.embedding_cache_ttl_days,
                },
            );
            let dirty_store = DirtySourceStore::new(memory.db());
            let fact_store = FactStore::new(memory.db());

            let mut enqueued = 0usize;
            if file_store.read_long_term().await.is_ok() {
                dirty_store
                    .enqueue(
                        &agent_id,
                        DIRTY_KIND_MEMORY_FILE,
                        "MEMORY.md",
                        "rebuild_index",
                    )
                    .await?;
                enqueued += 1;
            }

            for (date, _path) in file_store.list_daily_files().await? {
                dirty_store
                    .enqueue(
                        &agent_id,
                        DIRTY_KIND_DAILY_FILE,
                        &format!("memory/{}.md", date.format("%Y-%m-%d")),
                        "rebuild_index",
                    )
                    .await?;
                enqueued += 1;
            }

            for fact in fact_store.get_active_facts(&agent_id).await? {
                dirty_store
                    .enqueue(&agent_id, DIRTY_KIND_FACT, &fact.id, "rebuild_index")
                    .await?;
                enqueued += 1;
            }

            for session_id in session_reader.list_sessions().await? {
                dirty_store
                    .enqueue(&agent_id, DIRTY_KIND_SESSION, &session_id, "rebuild_index")
                    .await?;
                enqueued += 1;
            }

            let embedding_provider = build_embedding_provider(&config).await;
            let reindexed = search_index
                .process_dirty_sources(
                    &dirty_store,
                    &agent_id,
                    &file_store,
                    &session_reader,
                    embedding_provider.as_ref(),
                    usize::MAX,
                )
                .await?;

            println!(
                "Reindex complete for '{agent_id}': enqueued={enqueued}, reindexed_chunks={reindexed}"
            );
            Ok(())
        }
        MemoryCommands::Export { agent_id, format } => {
            let fact_store = clawhive_memory::fact_store::FactStore::new(memory.db());
            let facts = fact_store.get_active_facts(&agent_id).await?;

            let workspace_dir = root.join("workspaces").join(&agent_id);
            let file_store = clawhive_memory::file_store::MemoryFileStore::new(&workspace_dir);
            let long_term = file_store.read_long_term().await.unwrap_or_default();
            let daily_files = file_store.read_recent_daily(30).await.unwrap_or_default();

            let is_json = format.as_deref() != Some("markdown");

            if is_json {
                let export = serde_json::json!({
                    "agent_id": agent_id,
                    "facts": facts,
                    "long_term_memory": long_term,
                    "daily_files": daily_files.iter().map(|(date, content)| {
                        serde_json::json!({
                            "date": date.format("%Y-%m-%d").to_string(),
                            "content": content,
                        })
                    }).collect::<Vec<_>>(),
                });
                println!("{}", serde_json::to_string_pretty(&export)?);
            } else {
                println!("# Memory Export: {agent_id}\n");
                if !facts.is_empty() {
                    println!("## Facts ({} active)\n", facts.len());
                    for f in &facts {
                        println!(
                            "- [{}] {} (confidence: {:.1})",
                            f.fact_type, f.content, f.confidence
                        );
                    }
                    println!();
                }
                if !long_term.is_empty() {
                    println!("## MEMORY.md\n\n{long_term}\n");
                }
                for (date, content) in &daily_files {
                    println!("## {}\n\n{content}\n", date.format("%Y-%m-%d"));
                }
            }

            Ok(())
        }
    }
}

struct MemoryFileEntry {
    path: String,
    kind: MemoryFileKind,
    date: Option<NaiveDate>,
    content: String,
}

struct DailyCanonicalEntry {
    content: String,
    canonical_id: String,
}

async fn collect_memory_files(
    file_store: &MemoryFileStore,
    days: usize,
) -> Result<Vec<MemoryFileEntry>> {
    let mut files = Vec::new();
    let long_term = file_store.read_long_term().await?;
    if !long_term.is_empty() {
        files.push(MemoryFileEntry {
            path: "MEMORY.md".to_string(),
            kind: MemoryFileKind::LongTerm,
            date: None,
            content: long_term,
        });
    }

    let all_daily = file_store.list_daily_files().await?;
    let selected_daily = if days == 0 || all_daily.len() <= days {
        all_daily
    } else {
        all_daily.into_iter().take(days).collect()
    };

    for (date, _path) in selected_daily {
        if let Some(content) = file_store.read_daily(date).await? {
            files.push(MemoryFileEntry {
                path: format!("memory/{}.md", date.format("%Y-%m-%d")),
                kind: MemoryFileKind::Daily,
                date: Some(date),
                content,
            });
        }
    }

    Ok(files)
}

fn extract_daily_items(content: &str) -> Vec<String> {
    content
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            trimmed
                .strip_prefix("- ")
                .or_else(|| trimmed.strip_prefix("* "))
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned)
        })
        .collect()
}

fn find_best_canonical_match<'a>(
    item: &str,
    entries: &'a [DailyCanonicalEntry],
    threshold: f64,
) -> Option<&'a DailyCanonicalEntry> {
    let normalized_item = normalize_migration_text(item);
    if normalized_item.is_empty() {
        return None;
    }
    let item_tokens = tokenize_migration_text(&normalized_item);

    entries
        .iter()
        .filter_map(|entry| {
            let normalized_entry = normalize_migration_text(&entry.content);
            if normalized_entry.is_empty() {
                return None;
            }
            let similarity = if normalized_item.contains(&normalized_entry)
                || normalized_entry.contains(&normalized_item)
            {
                1.0
            } else {
                jaccard_similarity(&item_tokens, &tokenize_migration_text(&normalized_entry))
            };
            (similarity >= threshold).then_some((entry, similarity))
        })
        .max_by(|(_, left), (_, right)| left.total_cmp(right))
        .map(|(entry, _)| entry)
}

fn memory_entry_matches_fact(item: &str, fact: &str) -> bool {
    let normalized_item = normalize_migration_text(item);
    let normalized_fact = normalize_migration_text(fact);
    if normalized_item.is_empty() || normalized_fact.is_empty() {
        return false;
    }
    if normalized_item.contains(&normalized_fact) || normalized_fact.contains(&normalized_item) {
        return true;
    }
    jaccard_similarity(
        &tokenize_migration_text(&normalized_item),
        &tokenize_migration_text(&normalized_fact),
    ) >= 0.55
}

fn normalize_migration_text(input: &str) -> String {
    input
        .chars()
        .map(|ch| {
            if ch.is_alphanumeric() || ch.is_whitespace() {
                ch.to_ascii_lowercase()
            } else {
                ' '
            }
        })
        .collect::<String>()
}

fn tokenize_migration_text(input: &str) -> HashSet<String> {
    input
        .split_whitespace()
        .filter(|token| token.len() > 1)
        .map(ToOwned::to_owned)
        .collect()
}

fn jaccard_similarity(left: &HashSet<String>, right: &HashSet<String>) -> f64 {
    if left.is_empty() || right.is_empty() {
        return 0.0;
    }
    let overlap = left.intersection(right).count() as f64;
    overlap / left.union(right).count() as f64
}

fn summarize_trace_details(details: &str) -> String {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(details) {
        if let Some(diff_count) = value
            .get("diff")
            .and_then(|diff| diff.as_array())
            .map(|arr| arr.len())
        {
            let section = value
                .get("section")
                .and_then(|section| section.as_str())
                .unwrap_or("unknown");
            return format!("section={section}, diff_lines={diff_count}");
        }
        if let Some(obj) = value.as_object() {
            let keys = obj.keys().take(3).cloned().collect::<Vec<_>>().join(",");
            return format!("json keys: {keys}");
        }
    }

    let one_line = details.replace('\n', " ");
    one_line.chars().take(80).collect()
}

fn format_section_merge_detail(details: &serde_json::Value) -> Option<String> {
    let section = details.get("section")?.as_str()?;
    let Some(diff) = details.get("diff").and_then(|diff| diff.as_array()) else {
        return Some(format!("[{section}]\n  (no changes)"));
    };

    let mut lines = vec![format!("[{section}]")];
    if diff.is_empty() {
        lines.push("  (no changes)".to_string());
        return Some(lines.join("\n"));
    }

    for entry in diff {
        if let Some(line) = entry.as_str() {
            lines.push(format!("  {line}"));
        }
    }
    if lines.len() == 1 {
        lines.push("  (no changes)".to_string());
    }
    Some(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::{
        extract_daily_items, find_best_canonical_match, format_section_merge_detail,
        memory_entry_matches_fact, DailyCanonicalEntry,
    };

    #[test]
    fn extract_daily_items_only_keeps_bullets() {
        let content = "# 2026-03-29\n\n## General\n\n- Keep this\nparagraph\n* Keep that\n";
        let items = extract_daily_items(content);

        assert_eq!(
            items,
            vec!["Keep this".to_string(), "Keep that".to_string()]
        );
    }

    #[test]
    fn find_best_canonical_match_prefers_near_duplicate_entry() {
        let entries = vec![
            DailyCanonicalEntry {
                content: "Unrelated operational note".to_string(),
                canonical_id: "daily-1".to_string(),
            },
            DailyCanonicalEntry {
                content: "Use section-based consolidation for memory".to_string(),
                canonical_id: "daily-2".to_string(),
            },
        ];

        let matched = find_best_canonical_match(
            "Adopt section-based consolidation for memory refactor",
            &entries,
            0.4,
        )
        .expect("should match similar daily entry");

        assert_eq!(matched.canonical_id, "daily-2");
    }

    #[test]
    fn memory_entry_matches_fact_requires_meaningful_overlap() {
        assert!(memory_entry_matches_fact(
            "Use section-based consolidation for memory refactor",
            "Use section-based consolidation for memory"
        ));
        assert!(!memory_entry_matches_fact(
            "Runpod pod restarted overnight",
            "User prefers Chinese replies"
        ));
    }

    #[test]
    fn format_section_merge_detail_renders_diff_lines() {
        let details = serde_json::json!({
            "section": "关键历史决策",
            "diff": [
                "+ 放弃 A2A 协议，改用内部 sub-agent 方案",
                "+ 前端框架确定用 React 19 + Vite 6"
            ]
        });

        let rendered = format_section_merge_detail(&details).expect("render detail");
        assert!(rendered.contains("[关键历史决策]"));
        assert!(rendered.contains("+ 放弃 A2A 协议，改用内部 sub-agent 方案"));
        assert!(rendered.contains("+ 前端框架确定用 React 19 + Vite 6"));
    }
}
