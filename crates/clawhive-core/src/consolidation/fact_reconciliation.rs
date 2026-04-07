use anyhow::{anyhow, Result};
use chrono::{Duration, Utc};
use clawhive_memory::fact_store::{Fact, FactStore};
use clawhive_provider::LlmMessage;

use super::matching::fact_conflict_step_b_passes;
use super::text_utils::cosine_similarity;
use super::HippocampusConsolidator;

impl HippocampusConsolidator {
    pub(super) async fn reconcile_recent_fact_conflicts(&self) {
        let Some(memory_store) = &self.memory_store else {
            return;
        };

        let fact_store = FactStore::new(memory_store.db());
        let mut active_facts = match fact_store.get_active_facts(&self.agent_id).await {
            Ok(facts) => facts,
            Err(error) => {
                tracing::warn!(agent_id = %self.agent_id, error = %error, "failed to load active facts for consolidation reconciliation");
                return;
            }
        };

        if active_facts.len() < 2 {
            return;
        }

        let cutoff = (Utc::now() - Duration::hours(24)).to_rfc3339();
        let recent_facts = active_facts
            .iter()
            .filter(|fact| fact.created_at > cutoff)
            .cloned()
            .collect::<Vec<_>>();

        for recent in recent_facts {
            if active_facts.iter().all(|fact| fact.id != recent.id) {
                continue;
            }

            let others = active_facts
                .iter()
                .filter(|fact| fact.id != recent.id)
                .cloned()
                .collect::<Vec<_>>();
            if others.is_empty() {
                continue;
            }

            let (step_a, step_a_failed) = match self.find_conflicting_fact(&recent, &others).await {
                Ok(conflict) => (conflict, false),
                Err(error) => {
                    tracing::warn!(
                        agent_id = %self.agent_id,
                        recent_fact_id = %recent.id,
                        error = %error,
                        "embedding-based conflict check failed during consolidation reconciliation; falling back to step-B"
                    );
                    (None, true)
                }
            };

            let conflict = if self.embedding_provider.is_some() && !step_a_failed {
                step_a.filter(|candidate| fact_conflict_step_b_passes(&recent, candidate))
            } else {
                others
                    .into_iter()
                    .find(|candidate| fact_conflict_step_b_passes(&recent, candidate))
            };

            let Some(conflict) = conflict else {
                continue;
            };

            let confirmed = match self
                .confirm_fact_conflict_with_llm(&recent, &conflict)
                .await
            {
                Ok(true) => true,
                Ok(false) => {
                    tracing::info!(
                        agent_id = %self.agent_id,
                        recent_fact_id = %recent.id,
                        conflict_fact_id = %conflict.id,
                        "LLM rejected conflict candidate during reconciliation; skipping supersede"
                    );
                    false
                }
                Err(error) => {
                    tracing::warn!(
                        agent_id = %self.agent_id,
                        recent_fact_id = %recent.id,
                        conflict_fact_id = %conflict.id,
                        error = %error,
                        "LLM conflict confirmation failed; skipping supersede (conservative)"
                    );
                    false
                }
            };

            if !confirmed {
                continue;
            }

            if let Err(error) = self
                .supersede_with_existing_fact(&conflict, &recent, "auto_consolidation_reconcile")
                .await
            {
                tracing::warn!(
                    agent_id = %self.agent_id,
                    old_fact_id = %conflict.id,
                    replacement_fact_id = %recent.id,
                    error = %error,
                    "failed to auto-supersede fact during consolidation reconciliation"
                );
                continue;
            }

            tracing::info!(
                agent_id = %self.agent_id,
                old_fact_id = %conflict.id,
                replacement_fact_id = %recent.id,
                reason = "auto_consolidation_reconcile",
                "auto-superseded conflicting fact during 04:00 reconciliation"
            );

            active_facts.retain(|fact| fact.id != conflict.id);
        }
    }

    pub(super) async fn confirm_fact_conflict_with_llm(
        &self,
        recent: &Fact,
        candidate: &Fact,
    ) -> Result<bool> {
        let prompt = format!(
            "Compare these two facts about the same user:\n\n\
            Fact A (older): \"{}\"\nType: {}\n\n\
            Fact B (newer): \"{}\"\nType: {}\n\n\
            Question: Is Fact B a direct update or correction of Fact A? \
            (e.g. preference change, updated decision, corrected information)\n\n\
            Reply with exactly \"yes\" or \"no\". \
            Answer \"yes\" only if they are clearly about the same specific subject \
            and Fact B supersedes Fact A.",
            candidate.content, candidate.fact_type, recent.content, recent.fact_type
        );

        let model = self
            .model_compaction
            .as_deref()
            .unwrap_or(&self.model_primary);
        let response = self
            .router
            .chat(
                model,
                &self.model_fallbacks,
                None,
                vec![LlmMessage::user(prompt)],
                16,
            )
            .await?;

        let answer = response.text.trim().to_lowercase();
        Ok(answer.starts_with("yes"))
    }

    async fn supersede_with_existing_fact(
        &self,
        old_fact: &Fact,
        replacement_fact: &Fact,
        reason: &str,
    ) -> Result<()> {
        let Some(memory_store) = &self.memory_store else {
            return Err(anyhow!("memory store unavailable for fact reconciliation"));
        };

        let db = memory_store.db();
        let old_fact_id = old_fact.id.clone();
        let old_content = old_fact.content.clone();
        let replacement_id = replacement_fact.id.clone();
        let replacement_content = replacement_fact.content.clone();
        let reason = reason.to_string();

        tokio::task::spawn_blocking(move || -> Result<()> {
            let mut conn = db
                .lock()
                .map_err(|_| anyhow!("failed to lock sqlite connection"))?;
            let tx = conn.transaction()?;
            let now = Utc::now().to_rfc3339();
            let updated = tx.execute(
                "UPDATE facts SET status = 'superseded', superseded_by = ?1, updated_at = ?2 WHERE id = ?3 AND status = 'active'",
                [&replacement_id, &now, &old_fact_id],
            )?;
            if updated == 0 {
                tx.rollback()?;
                return Ok(());
            }

            tx.execute(
                "INSERT INTO fact_history (id, fact_id, event, old_content, new_content, reason, created_at) VALUES (?1, ?2, 'SUPERSEDE', ?3, ?4, ?5, ?6)",
                [
                    &format!(
                        "reconcile-{}-{}",
                        Utc::now().timestamp_nanos_opt().unwrap_or_default(),
                        replacement_id
                    ),
                    &old_fact_id,
                    &old_content,
                    &replacement_content,
                    &reason,
                    &now,
                ],
            )?;

            tx.commit()?;
            Ok(())
        })
        .await??;

        Ok(())
    }

    async fn find_conflicting_fact(
        &self,
        new_fact: &Fact,
        active_facts: &[Fact],
    ) -> Result<Option<Fact>> {
        let Some(provider) = &self.embedding_provider else {
            return Ok(None);
        };
        if active_facts.is_empty() {
            return Ok(None);
        }

        let mut texts = Vec::with_capacity(active_facts.len() + 1);
        texts.push(new_fact.content.clone());
        texts.extend(active_facts.iter().map(|fact| fact.content.clone()));

        let embeddings = provider.embed(&texts).await?.embeddings;
        if embeddings.len() != texts.len() {
            return Ok(None);
        }

        let new_embedding = &embeddings[0];
        let conflict = active_facts
            .iter()
            .zip(embeddings.iter().skip(1))
            .find(|(existing, embedding)| {
                existing.id != new_fact.id && cosine_similarity(new_embedding, embedding) > 0.85
            })
            .map(|(fact, _)| fact.clone());

        Ok(conflict)
    }
}
