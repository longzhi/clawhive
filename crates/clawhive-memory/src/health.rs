use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};
use chrono::Utc;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthReport {
    pub generated_at: String,
    pub facts: FactHealth,
    pub chunks: ChunkHealth,
    pub daily_files: DailyFileHealth,
    pub storage: StorageHealth,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FactHealth {
    pub total: usize,
    pub active: usize,
    pub superseded: usize,
    pub archived: usize,
    pub avg_confidence: f64,
    pub avg_salience: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkHealth {
    pub total: usize,
    pub with_embeddings: usize,
    pub orphan_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DailyFileHealth {
    pub active_count: usize,
    pub archived_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageHealth {
    pub db_size_bytes: u64,
}

pub struct HealthReporter {
    db: Arc<Mutex<Connection>>,
}

impl HealthReporter {
    pub fn new(db: Arc<Mutex<Connection>>) -> Self {
        Self { db }
    }

    pub async fn generate(
        &self,
        daily_active: usize,
        daily_archived: usize,
        orphan_count: usize,
    ) -> Result<HealthReport> {
        let db = Arc::clone(&self.db);
        tokio::task::spawn_blocking(move || {
            let conn = db.lock().map_err(|_| anyhow!("failed to lock db"))?;

            let (total, active, superseded, archived, avg_confidence, avg_salience):
                (usize, usize, usize, usize, f64, f64) = {
                    let mut stmt = conn.prepare(
                        "SELECT COUNT(*), \
                         COALESCE(SUM(CASE WHEN status = 'active' THEN 1 ELSE 0 END), 0), \
                         COALESCE(SUM(CASE WHEN status = 'superseded' THEN 1 ELSE 0 END), 0), \
                         COALESCE(SUM(CASE WHEN status = 'archived' THEN 1 ELSE 0 END), 0), \
                         COALESCE(AVG(confidence), 0.0), \
                         COALESCE(AVG(CAST(salience AS REAL)), 0.0) \
                         FROM facts",
                    )?;
                    stmt.query_row([], |row| {
                        Ok((
                            row.get::<_, usize>(0)?,
                            row.get::<_, usize>(1)?,
                            row.get::<_, usize>(2)?,
                            row.get::<_, usize>(3)?,
                            row.get::<_, f64>(4)?,
                            row.get::<_, f64>(5)?,
                        ))
                    })?
                };

            let (chunk_total, with_embeddings): (usize, usize) = {
                let mut stmt = conn.prepare(
                    "SELECT COUNT(*), COALESCE(SUM(CASE WHEN embedding != '' THEN 1 ELSE 0 END), 0) FROM chunks",
                )?;
                stmt.query_row([], |row| {
                    Ok((
                        row.get::<_, usize>(0)?,
                        row.get::<_, usize>(1)?,
                    ))
                })?
            };

            let db_size: u64 = conn
                .query_row(
                    "SELECT page_count * page_size FROM pragma_page_count(), pragma_page_size()",
                    [],
                    |row| row.get::<_, u64>(0),
                )
                .unwrap_or(0);

            Ok(HealthReport {
                generated_at: Utc::now().to_rfc3339(),
                facts: FactHealth {
                    total,
                    active,
                    superseded,
                    archived,
                    avg_confidence,
                    avg_salience,
                },
                chunks: ChunkHealth {
                    total: chunk_total,
                    with_embeddings,
                    orphan_count,
                },
                daily_files: DailyFileHealth {
                    active_count: daily_active,
                    archived_count: daily_archived,
                },
                storage: StorageHealth {
                    db_size_bytes: db_size,
                },
            })
        })
        .await?
    }
}

pub async fn write_health_report(report: &HealthReport, workspace_dir: &Path) -> Result<()> {
    let health_path = workspace_dir.join("memory").join("health.json");
    if let Some(parent) = health_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let json = serde_json::to_string_pretty(report)?;
    tokio::fs::write(&health_path, json).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use rusqlite::params;
    use tempfile::tempdir;

    use crate::migrations::run_migrations;

    fn setup_db() -> Result<Arc<Mutex<Connection>>> {
        let conn = Connection::open_in_memory()?;
        run_migrations(&conn)?;
        Ok(Arc::new(Mutex::new(conn)))
    }

    #[tokio::test]
    async fn generate_reports_expected_counts_with_seeded_data() {
        let db = setup_db().expect("setup db");
        {
            let conn = db
                .lock()
                .expect("lock sqlite connection");

            conn.execute(
                "INSERT INTO facts (
                    id, agent_id, content, fact_type, importance, confidence, salience,
                    status, recorded_at, source_type, access_count,
                    supersede_reason, affect, affect_intensity, created_at, updated_at
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
                params![
                    "f1",
                    "agent-a",
                    "fact one",
                    "rule",
                    0.8_f64,
                    0.9_f64,
                    80_i64,
                    "active",
                    "2026-04-05T00:00:00Z",
                    "session",
                    0_i64,
                    "",
                    "neutral",
                    0.0_f64,
                    "2026-04-05T00:00:00Z",
                    "2026-04-05T00:00:00Z",
                ],
            )
            .expect("insert fact f1");

            conn.execute(
                "INSERT INTO facts (
                    id, agent_id, content, fact_type, importance, confidence, salience,
                    status, recorded_at, source_type, access_count,
                    supersede_reason, affect, affect_intensity, created_at, updated_at
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
                params![
                    "f2",
                    "agent-a",
                    "fact two",
                    "event",
                    0.6_f64,
                    0.7_f64,
                    40_i64,
                    "superseded",
                    "2026-04-05T00:00:01Z",
                    "session",
                    0_i64,
                    "replaced",
                    "neutral",
                    0.0_f64,
                    "2026-04-05T00:00:01Z",
                    "2026-04-05T00:00:01Z",
                ],
            )
            .expect("insert fact f2");

            conn.execute(
                "INSERT INTO facts (
                    id, agent_id, content, fact_type, importance, confidence, salience,
                    status, recorded_at, source_type, access_count,
                    supersede_reason, affect, affect_intensity, created_at, updated_at
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
                params![
                    "f3",
                    "agent-a",
                    "fact three",
                    "preference",
                    0.4_f64,
                    0.6_f64,
                    20_i64,
                    "archived",
                    "2026-04-05T00:00:02Z",
                    "session",
                    0_i64,
                    "",
                    "neutral",
                    0.0_f64,
                    "2026-04-05T00:00:02Z",
                    "2026-04-05T00:00:02Z",
                ],
            )
            .expect("insert fact f3");

            conn.execute(
                "INSERT INTO chunks (
                    id, path, source, start_line, end_line, hash, model, text, embedding,
                    updated_at, access_count, agent_id, last_accessed, created_at
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
                params![
                    "c1",
                    "memory/2026-04-05.md",
                    "daily",
                    1_i64,
                    2_i64,
                    "hash-1",
                    "model-a",
                    "chunk one",
                    "[0.1,0.2]",
                    "2026-04-05T00:00:00Z",
                    1_i64,
                    "agent-a",
                    "2026-04-05T00:00:00Z",
                    "2026-04-05T00:00:00Z",
                ],
            )
            .expect("insert chunk c1");

            conn.execute(
                "INSERT INTO chunks (
                    id, path, source, start_line, end_line, hash, model, text, embedding,
                    updated_at, access_count, agent_id, last_accessed, created_at
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
                params![
                    "c2",
                    "memory/MEMORY.md",
                    "long_term",
                    3_i64,
                    4_i64,
                    "hash-2",
                    "model-a",
                    "chunk two",
                    "",
                    "2026-04-05T00:00:01Z",
                    0_i64,
                    "agent-a",
                    "2026-04-05T00:00:01Z",
                    "2026-04-05T00:00:01Z",
                ],
            )
            .expect("insert chunk c2");
        }

        let reporter = HealthReporter::new(db);
        let report = reporter.generate(3, 1, 2).await.expect("generate report");

        assert_eq!(report.facts.total, 3);
        assert_eq!(report.facts.active, 1);
        assert_eq!(report.facts.superseded, 1);
        assert_eq!(report.facts.archived, 1);
        assert!((report.facts.avg_confidence - 0.733333333).abs() < 1e-6);
        assert!((report.facts.avg_salience - 46.666666666).abs() < 1e-6);

        assert_eq!(report.chunks.total, 2);
        assert_eq!(report.chunks.with_embeddings, 1);
        assert_eq!(report.chunks.orphan_count, 2);

        assert_eq!(report.daily_files.active_count, 3);
        assert_eq!(report.daily_files.archived_count, 1);
    }

    #[tokio::test]
    async fn generate_returns_zeros_for_empty_database() {
        let reporter = HealthReporter::new(setup_db().expect("setup db"));
        let report = reporter
            .generate(0, 0, 0)
            .await
            .expect("generate empty report");

        assert_eq!(report.facts.total, 0);
        assert_eq!(report.facts.active, 0);
        assert_eq!(report.facts.superseded, 0);
        assert_eq!(report.facts.archived, 0);
        assert_eq!(report.facts.avg_confidence, 0.0);
        assert_eq!(report.facts.avg_salience, 0.0);

        assert_eq!(report.chunks.total, 0);
        assert_eq!(report.chunks.with_embeddings, 0);
        assert_eq!(report.chunks.orphan_count, 0);

        assert_eq!(report.daily_files.active_count, 0);
        assert_eq!(report.daily_files.archived_count, 0);
    }

    #[tokio::test]
    async fn write_health_report_persists_json_file() {
        let tmp = tempdir().expect("create tempdir");
        let report = HealthReport {
            generated_at: "2026-04-05T00:00:00Z".to_string(),
            facts: FactHealth {
                total: 0,
                active: 0,
                superseded: 0,
                archived: 0,
                avg_confidence: 0.0,
                avg_salience: 0.0,
            },
            chunks: ChunkHealth {
                total: 0,
                with_embeddings: 0,
                orphan_count: 0,
            },
            daily_files: DailyFileHealth {
                active_count: 0,
                archived_count: 0,
            },
            storage: StorageHealth { db_size_bytes: 0 },
        };

        write_health_report(&report, tmp.path())
            .await
            .expect("write health report");

        let content = tokio::fs::read_to_string(tmp.path().join("memory").join("health.json"))
            .await
            .expect("read health report");
        let parsed: HealthReport = serde_json::from_str(&content).expect("parse health report");
        assert_eq!(parsed.generated_at, "2026-04-05T00:00:00Z");
    }
}
