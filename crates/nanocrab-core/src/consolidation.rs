use super::router::LlmRouter;
use anyhow::Result;
use chrono::Utc;
use nanocrab_memory::{
    Concept, ConceptStatus, ConceptType, Episode, Link, LinkRelation, MemoryStore,
};
use nanocrab_provider::LlmMessage;
use std::sync::Arc;
use uuid::Uuid;

pub struct Consolidator {
    memory: Arc<MemoryStore>,
    router: Arc<LlmRouter>,
    model_primary: String,
    model_fallbacks: Vec<String>,
}

#[derive(Debug)]
pub struct ConsolidationReport {
    pub concepts_created: usize,
    pub concepts_updated: usize,
    pub episodes_processed: usize,
    pub concepts_staled: usize,
    pub episodes_purged: usize,
}

impl Consolidator {
    pub fn new(
        memory: Arc<MemoryStore>,
        router: Arc<LlmRouter>,
        model_primary: String,
        model_fallbacks: Vec<String>,
    ) -> Self {
        Self {
            memory,
            router,
            model_primary,
            model_fallbacks,
        }
    }

    pub async fn run_daily(&self) -> Result<ConsolidationReport> {
        let episodes = self.memory.search_episodes("", 1, 100).await?;
        let high_value: Vec<_> = episodes
            .into_iter()
            .filter(|e| e.importance >= 0.6)
            .collect();

        let mut created = 0;
        let mut updated = 0;

        if !high_value.is_empty() {
            let candidates = self.extract_concepts(&high_value).await?;

            for (concept, source_episode_id) in candidates {
                let existing = self.memory.find_concept_by_key(&concept.key).await?;
                if existing.is_some() {
                    updated += 1;
                } else {
                    created += 1;
                }
                self.memory.upsert_concept(concept.clone()).await?;

                let link = Link {
                    id: Uuid::new_v4(),
                    episode_id: source_episode_id,
                    concept_id: concept.id,
                    relation: LinkRelation::Supports,
                    created_at: Utc::now(),
                };
                self.memory.insert_link(link).await?;
            }
        }

        let concepts_staled = self.memory.mark_stale_concepts(30).await?;
        let episodes_purged = self.memory.purge_old_episodes(90).await?;

        Ok(ConsolidationReport {
            concepts_created: created,
            concepts_updated: updated,
            episodes_processed: high_value.len(),
            concepts_staled,
            episodes_purged,
        })
    }

    async fn extract_concepts(&self, episodes: &[Episode]) -> Result<Vec<(Concept, Uuid)>> {
        let episodes_text = episodes
            .iter()
            .map(|e| format!("[{}] {}: {}", e.ts.format("%m-%d"), e.speaker, e.text))
            .collect::<Vec<_>>()
            .join("\n");

        let system = "You are a knowledge extractor. From the following conversations, extract stable facts, preferences, and rules.\
            Output a JSON array, each item format: {\"type\":\"fact|preference|rule\", \"key\":\"short_identifier\", \"value\":\"description\", \"confidence\":0.0-1.0, \"source_index\":0}\
            Only extract high-confidence stable knowledge, ignore temporary content.".to_string();

        let messages = vec![LlmMessage {
            role: "user".into(),
            content: episodes_text,
        }];

        let resp = self
            .router
            .chat(
                &self.model_primary,
                &self.model_fallbacks,
                Some(system),
                messages,
                2048,
            )
            .await?;

        parse_concept_candidates(&resp.text, episodes)
    }
}

fn parse_concept_candidates(
    llm_output: &str,
    episodes: &[Episode],
) -> Result<Vec<(Concept, Uuid)>> {
    let json_str = llm_output
        .trim()
        .strip_prefix("```json")
        .or_else(|| llm_output.trim().strip_prefix("```"))
        .unwrap_or(llm_output.trim());
    let json_str = json_str.strip_suffix("```").unwrap_or(json_str).trim();

    let items: Vec<serde_json::Value> = match serde_json::from_str(json_str) {
        Ok(v) => v,
        Err(_) => return Ok(vec![]),
    };

    let now = Utc::now();
    let mut results = Vec::new();

    for item in items {
        let concept_type = match item.get("type").and_then(|v| v.as_str()) {
            Some("fact") => ConceptType::Fact,
            Some("preference") => ConceptType::Preference,
            Some("rule") => ConceptType::Rule,
            _ => continue,
        };

        let key = match item.get("key").and_then(|v| v.as_str()) {
            Some(k) => k.to_string(),
            None => continue,
        };

        let value = match item.get("value").and_then(|v| v.as_str()) {
            Some(v) => v.to_string(),
            None => continue,
        };

        let confidence = item
            .get("confidence")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.5) as f32;

        let source_index = item
            .get("source_index")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize;

        let source_episode_id = episodes
            .get(source_index)
            .map(|e| e.id)
            .unwrap_or_else(|| episodes[0].id);

        let concept = Concept {
            id: Uuid::new_v4(),
            concept_type,
            key,
            value,
            confidence,
            evidence: vec![format!("episode:{}", source_episode_id)],
            first_seen: now,
            last_verified: now,
            status: ConceptStatus::Active,
        };

        results.push((concept, source_episode_id));
    }

    Ok(results)
}

pub struct ConsolidationScheduler {
    consolidator: Arc<Consolidator>,
    interval_hours: u64,
}

impl ConsolidationScheduler {
    pub fn new(consolidator: Arc<Consolidator>, interval_hours: u64) -> Self {
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
                tracing::info!("Running scheduled consolidation...");
                match self.consolidator.run_daily().await {
                    Ok(report) => {
                        tracing::info!(
                            "Consolidation complete: {} created, {} updated, {} processed, {} staled, {} purged",
                            report.concepts_created,
                            report.concepts_updated,
                            report.episodes_processed,
                            report.concepts_staled,
                            report.episodes_purged
                        );
                    }
                    Err(e) => {
                        tracing::error!("Consolidation failed: {e}");
                    }
                }
            }
        })
    }

    pub async fn run_once(&self) -> Result<ConsolidationReport> {
        self.consolidator.run_daily().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_episode(text: &str, importance: f32) -> Episode {
        Episode {
            id: Uuid::new_v4(),
            ts: Utc::now(),
            session_id: "test:session".into(),
            speaker: "user".into(),
            text: text.into(),
            tags: vec![],
            importance,
            context_hash: None,
            source_ref: None,
        }
    }

    #[test]
    fn parse_concept_candidates_valid_json() {
        let episodes = vec![make_episode("I like Rust", 0.8)];
        let json = r#"[{"type":"preference","key":"lang.preferred","value":"Rust","confidence":0.9,"source_index":0}]"#;
        let result = parse_concept_candidates(json, &episodes).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0.key, "lang.preferred");
        assert_eq!(result[0].0.concept_type, ConceptType::Preference);
    }

    #[test]
    fn parse_concept_candidates_with_code_fence() {
        let episodes = vec![make_episode("test", 0.8)];
        let json = "```json\n[{\"type\":\"fact\",\"key\":\"test.key\",\"value\":\"test value\",\"confidence\":0.8,\"source_index\":0}]\n```";
        let result = parse_concept_candidates(json, &episodes).unwrap();
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn parse_concept_candidates_invalid_json() {
        let episodes = vec![make_episode("test", 0.8)];
        let result = parse_concept_candidates("not json at all", &episodes).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn parse_concept_candidates_missing_fields_skipped() {
        let episodes = vec![make_episode("test", 0.8)];
        let json = r#"[{"type":"fact"},{"type":"fact","key":"k","value":"v","confidence":0.9,"source_index":0}]"#;
        let result = parse_concept_candidates(json, &episodes).unwrap();
        assert_eq!(result.len(), 1);
    }
}
