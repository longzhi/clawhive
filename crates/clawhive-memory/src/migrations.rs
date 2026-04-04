use anyhow::Result;
use rusqlite::Connection;
use std::collections::HashSet;

type Migration = (i64, &'static str);

fn migrations() -> Vec<Migration> {
    vec![
        (
            1,
            r#"
            CREATE TABLE IF NOT EXISTS episodes (
                id TEXT PRIMARY KEY,
                ts TEXT NOT NULL,
                session_id TEXT NOT NULL,
                speaker TEXT NOT NULL,
                text TEXT NOT NULL,
                tags TEXT NOT NULL,
                importance REAL NOT NULL,
                context_hash TEXT,
                source_ref TEXT
            );

            CREATE INDEX IF NOT EXISTS idx_episodes_session_ts ON episodes(session_id, ts DESC);
            CREATE INDEX IF NOT EXISTS idx_episodes_ts ON episodes(ts DESC);
            "#,
        ),
        (
            2,
            r#"
            CREATE TABLE IF NOT EXISTS concepts (
                id TEXT PRIMARY KEY,
                concept_type TEXT NOT NULL,
                key TEXT NOT NULL UNIQUE,
                value TEXT NOT NULL,
                confidence REAL NOT NULL,
                evidence TEXT NOT NULL,
                first_seen TEXT NOT NULL,
                last_verified TEXT NOT NULL,
                status TEXT NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_concepts_type ON concepts(concept_type);
            CREATE INDEX IF NOT EXISTS idx_concepts_status ON concepts(status);
            CREATE INDEX IF NOT EXISTS idx_concepts_last_verified ON concepts(last_verified DESC);
            "#,
        ),
        (
            3,
            r#"
            CREATE TABLE IF NOT EXISTS links (
                id TEXT PRIMARY KEY,
                episode_id TEXT NOT NULL,
                concept_id TEXT NOT NULL,
                relation TEXT NOT NULL,
                created_at TEXT NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_links_episode_id ON links(episode_id);
            CREATE INDEX IF NOT EXISTS idx_links_concept_id ON links(concept_id);
            "#,
        ),
        (
            4,
            r#"
            CREATE TABLE IF NOT EXISTS sessions (
                session_key TEXT PRIMARY KEY,
                agent_id TEXT NOT NULL,
                created_at TEXT NOT NULL,
                last_active TEXT NOT NULL,
                ttl_seconds INTEGER NOT NULL
            );
            "#,
        ),
        (
            5,
            r#"
            CREATE TABLE IF NOT EXISTS meta (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS files (
                path TEXT PRIMARY KEY,
                source TEXT NOT NULL,
                hash TEXT NOT NULL,
                mtime INTEGER NOT NULL,
                size INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS chunks (
                id TEXT PRIMARY KEY,
                path TEXT NOT NULL,
                source TEXT NOT NULL,
                start_line INTEGER NOT NULL,
                end_line INTEGER NOT NULL,
                hash TEXT NOT NULL,
                model TEXT NOT NULL DEFAULT '',
                text TEXT NOT NULL,
                embedding TEXT NOT NULL DEFAULT '',
                updated_at INTEGER NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_chunks_path ON chunks(path);
            CREATE INDEX IF NOT EXISTS idx_chunks_source ON chunks(source);

            CREATE TABLE IF NOT EXISTS embedding_cache (
                provider TEXT NOT NULL,
                model TEXT NOT NULL,
                provider_key TEXT NOT NULL,
                hash TEXT NOT NULL,
                embedding TEXT NOT NULL,
                dims INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                PRIMARY KEY (provider, model, provider_key, hash)
            );

            CREATE VIRTUAL TABLE IF NOT EXISTS chunks_fts USING fts5(
                text,
                id UNINDEXED,
                path UNINDEXED,
                source UNINDEXED,
                model UNINDEXED,
                start_line UNINDEXED,
                end_line UNINDEXED
            );
            "#,
        ),
        (
            6,
            r#"
            CREATE TABLE IF NOT EXISTS memory_trace (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp TEXT NOT NULL DEFAULT (datetime('now')),
                agent_id TEXT NOT NULL,
                operation TEXT NOT NULL,
                details TEXT NOT NULL DEFAULT '{}',
                duration_ms INTEGER
            );

            CREATE INDEX IF NOT EXISTS idx_memory_trace_agent ON memory_trace(agent_id);
            CREATE INDEX IF NOT EXISTS idx_memory_trace_op ON memory_trace(agent_id, operation);
            CREATE INDEX IF NOT EXISTS idx_memory_trace_ts ON memory_trace(timestamp DESC);

            ALTER TABLE chunks ADD COLUMN access_count INTEGER NOT NULL DEFAULT 0;
            "#,
        ),
        (
            7,
            r#"
            CREATE TABLE IF NOT EXISTS facts (
                id             TEXT PRIMARY KEY,
                agent_id       TEXT NOT NULL,
                content        TEXT NOT NULL,
                fact_type      TEXT NOT NULL,
                importance     REAL NOT NULL DEFAULT 0.5,
                confidence     REAL NOT NULL DEFAULT 1.0,
                status         TEXT NOT NULL DEFAULT 'active',
                occurred_at    TEXT,
                recorded_at    TEXT NOT NULL,
                source_type    TEXT NOT NULL,
                source_session TEXT,
                access_count   INTEGER NOT NULL DEFAULT 0,
                last_accessed  TEXT,
                superseded_by  TEXT,
                created_at     TEXT NOT NULL,
                updated_at     TEXT NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_facts_agent ON facts(agent_id);
            CREATE INDEX IF NOT EXISTS idx_facts_agent_status ON facts(agent_id, status);
            CREATE INDEX IF NOT EXISTS idx_facts_type ON facts(agent_id, fact_type);

            CREATE TABLE IF NOT EXISTS fact_history (
                id             TEXT PRIMARY KEY,
                fact_id        TEXT NOT NULL,
                event          TEXT NOT NULL,
                old_content    TEXT,
                new_content    TEXT,
                reason         TEXT,
                created_at     TEXT NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_fact_history_fact ON fact_history(fact_id);
            "#,
        ),
        (
            8,
            r#"
            ALTER TABLE sessions ADD COLUMN interaction_count INTEGER NOT NULL DEFAULT 0;
            "#,
        ),
        (
            9,
            r#"
            ALTER TABLE chunks ADD COLUMN agent_id TEXT NOT NULL DEFAULT '';
            CREATE INDEX IF NOT EXISTS idx_chunks_agent_id ON chunks(agent_id);
            "#,
        ),
        (
            10,
            r#"
            DELETE FROM chunks WHERE agent_id = '';
            DELETE FROM chunks_fts WHERE rowid NOT IN (SELECT rowid FROM chunks);
            "#,
        ),
        (
            11,
            r#"
            CREATE TABLE IF NOT EXISTS files_v2 (
                agent_id TEXT NOT NULL,
                path TEXT NOT NULL,
                source TEXT NOT NULL,
                hash TEXT NOT NULL,
                mtime INTEGER NOT NULL,
                size INTEGER NOT NULL,
                PRIMARY KEY (agent_id, path)
            );

            INSERT INTO files_v2(agent_id, path, source, hash, mtime, size)
            SELECT DISTINCT
                COALESCE(NULLIF(chunks.agent_id, ''), '') AS agent_id,
                files.path,
                files.source,
                files.hash,
                files.mtime,
                files.size
            FROM files
            LEFT JOIN chunks ON chunks.path = files.path;

            DROP TABLE files;
            ALTER TABLE files_v2 RENAME TO files;
            CREATE INDEX IF NOT EXISTS idx_files_agent_source ON files(agent_id, source);
            "#,
        ),
        (
            12,
            r#"
            CREATE TABLE IF NOT EXISTS memory_canon (
                canonical_id TEXT PRIMARY KEY,
                agent_id TEXT NOT NULL,
                canonical_kind TEXT NOT NULL,
                summary TEXT NOT NULL,
                status TEXT NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_memory_canon_agent_status
            ON memory_canon(agent_id, status);

            CREATE TABLE IF NOT EXISTS memory_lineage (
                id TEXT PRIMARY KEY,
                agent_id TEXT NOT NULL,
                canonical_id TEXT NOT NULL,
                source_kind TEXT NOT NULL,
                source_ref TEXT NOT NULL,
                relation TEXT NOT NULL,
                created_at TEXT NOT NULL,
                FOREIGN KEY (canonical_id) REFERENCES memory_canon(canonical_id)
            );

            CREATE INDEX IF NOT EXISTS idx_memory_lineage_agent_canonical
            ON memory_lineage(agent_id, canonical_id);

            CREATE INDEX IF NOT EXISTS idx_memory_lineage_source
            ON memory_lineage(agent_id, source_kind, source_ref);
            "#,
        ),
        (
            13,
            r#"
            CREATE TABLE IF NOT EXISTS dirty_sources (
                id TEXT PRIMARY KEY,
                agent_id TEXT NOT NULL,
                source_kind TEXT NOT NULL,
                source_ref TEXT NOT NULL,
                reason TEXT NOT NULL,
                created_at TEXT NOT NULL,
                processed_at TEXT
            );

            CREATE UNIQUE INDEX IF NOT EXISTS idx_dirty_sources_unique
            ON dirty_sources(agent_id, source_kind, source_ref);

            CREATE INDEX IF NOT EXISTS idx_dirty_sources_pending
            ON dirty_sources(agent_id, processed_at, created_at);
            "#,
        ),
        (
            14,
            r#"
            CREATE TABLE IF NOT EXISTS session_memory_state (
                agent_id TEXT NOT NULL,
                session_id TEXT NOT NULL DEFAULT '',
                session_key TEXT NOT NULL,
                last_flushed_turn INTEGER NOT NULL DEFAULT 0,
                last_boundary_flush_at TEXT,
                pending_flush INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at TEXT NOT NULL DEFAULT (datetime('now')),
                PRIMARY KEY (agent_id, session_id)
            );

            CREATE INDEX IF NOT EXISTS idx_session_memory_state_pending
            ON session_memory_state(agent_id, pending_flush, updated_at);
            "#,
        ),
        (
            15,
            r#"
            ALTER TABLE sessions ADD COLUMN session_id TEXT NOT NULL DEFAULT '';
            UPDATE sessions
            SET session_id = session_key
            WHERE session_id = '';

            CREATE INDEX IF NOT EXISTS idx_sessions_agent_session_id
            ON sessions(agent_id, session_id);
            "#,
        ),
        (
            16,
            r#"
            CREATE TABLE IF NOT EXISTS session_memory_state_v2 (
                agent_id TEXT NOT NULL,
                session_id TEXT NOT NULL,
                session_key TEXT NOT NULL,
                last_flushed_turn INTEGER NOT NULL DEFAULT 0,
                last_boundary_flush_at TEXT,
                pending_flush INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at TEXT NOT NULL DEFAULT (datetime('now')),
                PRIMARY KEY (agent_id, session_id)
            );

            INSERT INTO session_memory_state_v2 (
                agent_id,
                session_id,
                session_key,
                last_flushed_turn,
                last_boundary_flush_at,
                pending_flush,
                created_at,
                updated_at
            )
            SELECT
                agent_id,
                CASE WHEN session_id = '' THEN session_key ELSE session_id END,
                session_key,
                last_flushed_turn,
                last_boundary_flush_at,
                pending_flush,
                created_at,
                updated_at
            FROM session_memory_state;

            DROP TABLE session_memory_state;
            ALTER TABLE session_memory_state_v2 RENAME TO session_memory_state;

            CREATE INDEX IF NOT EXISTS idx_session_memory_state_pending
            ON session_memory_state(agent_id, pending_flush, updated_at);
            CREATE INDEX IF NOT EXISTS idx_session_memory_state_key
            ON session_memory_state(agent_id, session_key);
            "#,
        ),
        (
            17,
            r#"
            ALTER TABLE session_memory_state
            ADD COLUMN recent_explicit_writes TEXT NOT NULL DEFAULT '[]';
            "#,
        ),
        (
            18,
            r#"
            ALTER TABLE session_memory_state
            ADD COLUMN open_episodes TEXT NOT NULL DEFAULT '[]';
            "#,
        ),
        (
            19,
            r#"
            DROP TABLE IF EXISTS links;
            DROP TABLE IF EXISTS concepts;
            DROP TABLE IF EXISTS episodes;
            "#,
        ),
        (
            20,
            r#"
            UPDATE chunks SET updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', updated_at, 'unixepoch')
            WHERE typeof(updated_at) = 'integer';

            UPDATE embedding_cache SET updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', updated_at, 'unixepoch')
            WHERE typeof(updated_at) = 'integer';
            "#,
        ),
        (
            21,
            r#"
            ALTER TABLE chunks ADD COLUMN last_accessed TEXT;
            "#,
        ),
        (
            22,
            r#"
            UPDATE chunks
            SET last_accessed = updated_at
            WHERE last_accessed IS NULL;
            "#,
        ),
        (
            23,
            r#"
            ALTER TABLE facts ADD COLUMN salience INTEGER NOT NULL DEFAULT 50;
            ALTER TABLE facts ADD COLUMN supersede_reason TEXT;
            "#,
        ),
        (
            24,
            r#"
            ALTER TABLE session_memory_state ADD COLUMN flush_phase TEXT NOT NULL DEFAULT 'idle';
            ALTER TABLE session_memory_state ADD COLUMN flush_phase_updated_at TEXT DEFAULT NULL;
            ALTER TABLE session_memory_state ADD COLUMN flush_summary_cache TEXT DEFAULT NULL;
            UPDATE session_memory_state
            SET flush_phase = 'idle'
            WHERE flush_phase IS NULL OR flush_phase = '';
            "#,
        ),
        (
            25,
            r#"
            ALTER TABLE chunks ADD COLUMN created_at TEXT NOT NULL DEFAULT '';

            UPDATE chunks
            SET updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', CAST(updated_at AS INTEGER), 'unixepoch')
            WHERE typeof(updated_at) = 'integer'
               OR (
                    trim(updated_at) <> ''
                    AND trim(updated_at) GLOB '[0-9]*'
                    AND instr(trim(updated_at), 'T') = 0
               );

            UPDATE chunks
            SET created_at = CASE
                WHEN trim(updated_at) = '' THEN strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
                WHEN trim(updated_at) GLOB '[0-9]*' AND instr(trim(updated_at), 'T') = 0
                    THEN strftime('%Y-%m-%dT%H:%M:%SZ', CAST(updated_at AS INTEGER), 'unixepoch')
                ELSE updated_at
            END
            WHERE trim(created_at) = '';
            "#,
        ),
        (
            26,
            r#"
            ALTER TABLE facts ADD COLUMN affect TEXT NOT NULL DEFAULT 'neutral';
            ALTER TABLE facts ADD COLUMN affect_intensity REAL NOT NULL DEFAULT 0.0;
            "#,
        ),
    ]
}

pub fn run_migrations(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS __schema_version (
            version INTEGER PRIMARY KEY,
            applied_at TEXT NOT NULL DEFAULT (datetime('now'))
        );
        "#,
    )?;

    let mut stmt = conn.prepare("SELECT version FROM __schema_version")?;
    let rows = stmt.query_map([], |row| row.get::<_, i64>(0))?;
    let mut applied = HashSet::new();
    for row in rows {
        applied.insert(row?);
    }

    for (version, sql) in migrations() {
        if applied.contains(&version) {
            continue;
        }

        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(sql)?;
        tx.execute(
            "INSERT INTO __schema_version(version, applied_at) VALUES (?1, datetime('now'))",
            [version],
        )?;
        tx.commit()?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::store::MemoryStore;

    #[test]
    fn migration_22_backfills_null_last_accessed_from_updated_at() -> Result<()> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(
            r#"
            CREATE TABLE chunks (
                id TEXT PRIMARY KEY,
                path TEXT NOT NULL,
                source TEXT NOT NULL,
                start_line INTEGER NOT NULL,
                end_line INTEGER NOT NULL,
                hash TEXT NOT NULL,
                model TEXT NOT NULL DEFAULT '',
                text TEXT NOT NULL,
                embedding TEXT NOT NULL DEFAULT '',
                updated_at TEXT NOT NULL,
                access_count INTEGER NOT NULL DEFAULT 0,
                agent_id TEXT NOT NULL DEFAULT '',
                last_accessed TEXT
            );

            CREATE TABLE facts (
                id             TEXT PRIMARY KEY,
                agent_id       TEXT NOT NULL,
                content        TEXT NOT NULL,
                fact_type      TEXT NOT NULL,
                importance     REAL NOT NULL DEFAULT 0.5,
                confidence     REAL NOT NULL DEFAULT 1.0,
                status         TEXT NOT NULL DEFAULT 'active',
                occurred_at    TEXT,
                recorded_at    TEXT NOT NULL,
                source_type    TEXT NOT NULL,
                source_session TEXT,
                access_count   INTEGER NOT NULL DEFAULT 0,
                last_accessed  TEXT,
                superseded_by  TEXT,
                created_at     TEXT NOT NULL,
                updated_at     TEXT NOT NULL
            );

            CREATE TABLE session_memory_state (
                agent_id TEXT NOT NULL,
                session_id TEXT NOT NULL,
                session_key TEXT NOT NULL,
                last_flushed_turn INTEGER NOT NULL DEFAULT 0,
                last_boundary_flush_at TEXT,
                pending_flush INTEGER NOT NULL DEFAULT 0,
                recent_explicit_writes TEXT NOT NULL DEFAULT '[]',
                open_episodes TEXT NOT NULL DEFAULT '[]',
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at TEXT NOT NULL DEFAULT (datetime('now')),
                PRIMARY KEY (agent_id, session_id)
            );

            CREATE TABLE __schema_version (
                version INTEGER PRIMARY KEY,
                applied_at TEXT NOT NULL DEFAULT (datetime('now'))
            );

            WITH RECURSIVE versions(v) AS (
                SELECT 1
                UNION ALL
                SELECT v + 1 FROM versions WHERE v < 21
            )
            INSERT INTO __schema_version(version, applied_at)
            SELECT v, datetime('now') FROM versions;
            "#,
        )?;

        conn.execute(
            "INSERT INTO chunks(id, path, source, start_line, end_line, hash, model, text, embedding, updated_at, access_count, agent_id, last_accessed)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, '', ?7, '', ?8, 0, '', NULL)",
            rusqlite::params![
                "c1",
                "MEMORY.md",
                "long_term",
                1,
                2,
                "h1",
                "text",
                "2026-04-01T10:00:00Z"
            ],
        )?;

        run_migrations(&conn)?;

        let last_accessed: String = conn.query_row(
            "SELECT last_accessed FROM chunks WHERE id = 'c1'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(last_accessed, "2026-04-01T10:00:00Z");

        Ok(())
    }

    #[test]
    fn migration_23_adds_salience_and_supersede_reason_without_data_loss() -> Result<()> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(
            r#"
            CREATE TABLE facts (
                id             TEXT PRIMARY KEY,
                agent_id       TEXT NOT NULL,
                content        TEXT NOT NULL,
                fact_type      TEXT NOT NULL,
                importance     REAL NOT NULL DEFAULT 0.5,
                confidence     REAL NOT NULL DEFAULT 1.0,
                status         TEXT NOT NULL DEFAULT 'active',
                occurred_at    TEXT,
                recorded_at    TEXT NOT NULL,
                source_type    TEXT NOT NULL,
                source_session TEXT,
                access_count   INTEGER NOT NULL DEFAULT 0,
                last_accessed  TEXT,
                superseded_by  TEXT,
                created_at     TEXT NOT NULL,
                updated_at     TEXT NOT NULL
            );

            CREATE TABLE session_memory_state (
                agent_id TEXT NOT NULL,
                session_id TEXT NOT NULL,
                session_key TEXT NOT NULL,
                last_flushed_turn INTEGER NOT NULL DEFAULT 0,
                last_boundary_flush_at TEXT,
                pending_flush INTEGER NOT NULL DEFAULT 0,
                recent_explicit_writes TEXT NOT NULL DEFAULT '[]',
                open_episodes TEXT NOT NULL DEFAULT '[]',
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at TEXT NOT NULL DEFAULT (datetime('now')),
                PRIMARY KEY (agent_id, session_id)
            );

            CREATE TABLE chunks (
                id TEXT PRIMARY KEY,
                path TEXT NOT NULL,
                source TEXT NOT NULL,
                start_line INTEGER NOT NULL,
                end_line INTEGER NOT NULL,
                hash TEXT NOT NULL,
                model TEXT NOT NULL DEFAULT '',
                text TEXT NOT NULL,
                embedding TEXT NOT NULL DEFAULT '',
                updated_at TEXT NOT NULL,
                access_count INTEGER NOT NULL DEFAULT 0,
                agent_id TEXT NOT NULL DEFAULT '',
                last_accessed TEXT
            );

            CREATE TABLE __schema_version (
                version INTEGER PRIMARY KEY,
                applied_at TEXT NOT NULL DEFAULT (datetime('now'))
            );

            WITH RECURSIVE versions(v) AS (
                SELECT 1
                UNION ALL
                SELECT v + 1 FROM versions WHERE v < 22
            )
            INSERT INTO __schema_version(version, applied_at)
            SELECT v, datetime('now') FROM versions;
            "#,
        )?;

        conn.execute(
            "INSERT INTO facts(id, agent_id, content, fact_type, importance, confidence, status, occurred_at, recorded_at, source_type, source_session, access_count, last_accessed, superseded_by, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, 0.9, 0.8, 'active', NULL, ?5, 'manual', NULL, 0, NULL, NULL, ?5, ?5)",
            rusqlite::params![
                "f1",
                "agent-1",
                "existing fact",
                "preference",
                "2026-04-01T10:00:00Z"
            ],
        )?;

        run_migrations(&conn)?;

        let (content, salience, supersede_reason): (String, i64, Option<String>) = conn.query_row(
            "SELECT content, salience, supersede_reason FROM facts WHERE id = 'f1'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        assert_eq!(content, "existing fact");
        assert_eq!(salience, 50);
        assert_eq!(supersede_reason, None);

        Ok(())
    }

    #[test]
    fn migration_24_adds_flush_phase_columns_and_backfills_idle() -> Result<()> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(
            r#"
            CREATE TABLE session_memory_state (
                agent_id TEXT NOT NULL,
                session_id TEXT NOT NULL,
                session_key TEXT NOT NULL,
                last_flushed_turn INTEGER NOT NULL DEFAULT 0,
                last_boundary_flush_at TEXT,
                pending_flush INTEGER NOT NULL DEFAULT 0,
                recent_explicit_writes TEXT NOT NULL DEFAULT '[]',
                open_episodes TEXT NOT NULL DEFAULT '[]',
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at TEXT NOT NULL DEFAULT (datetime('now')),
                PRIMARY KEY (agent_id, session_id)
            );

            CREATE TABLE chunks (
                id TEXT PRIMARY KEY,
                path TEXT NOT NULL,
                source TEXT NOT NULL,
                start_line INTEGER NOT NULL,
                end_line INTEGER NOT NULL,
                hash TEXT NOT NULL,
                model TEXT NOT NULL DEFAULT '',
                text TEXT NOT NULL,
                embedding TEXT NOT NULL DEFAULT '',
                updated_at TEXT NOT NULL,
                access_count INTEGER NOT NULL DEFAULT 0,
                agent_id TEXT NOT NULL DEFAULT '',
                last_accessed TEXT
            );

            CREATE TABLE facts (
                id             TEXT PRIMARY KEY,
                agent_id       TEXT NOT NULL,
                content        TEXT NOT NULL,
                fact_type      TEXT NOT NULL,
                importance     REAL NOT NULL DEFAULT 0.5,
                confidence     REAL NOT NULL DEFAULT 1.0,
                status         TEXT NOT NULL DEFAULT 'active',
                occurred_at    TEXT,
                recorded_at    TEXT NOT NULL,
                source_type    TEXT NOT NULL,
                source_session TEXT,
                access_count   INTEGER NOT NULL DEFAULT 0,
                last_accessed  TEXT,
                superseded_by  TEXT,
                salience       INTEGER NOT NULL DEFAULT 50,
                supersede_reason TEXT,
                created_at     TEXT NOT NULL,
                updated_at     TEXT NOT NULL
            );

            CREATE TABLE __schema_version (
                version INTEGER PRIMARY KEY,
                applied_at TEXT NOT NULL DEFAULT (datetime('now'))
            );

            WITH RECURSIVE versions(v) AS (
                SELECT 1
                UNION ALL
                SELECT v + 1 FROM versions WHERE v < 23
            )
            INSERT INTO __schema_version(version, applied_at)
            SELECT v, datetime('now') FROM versions;
            "#,
        )?;

        conn.execute(
            "INSERT INTO session_memory_state(agent_id, session_id, session_key, last_flushed_turn, pending_flush, recent_explicit_writes, open_episodes)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params!["agent-1", "session-1", "chat-1", 12_i64, 1_i64, "[]", "[]"],
        )?;

        run_migrations(&conn)?;

        let (flush_phase, updated_at, summary_cache): (String, Option<String>, Option<String>) =
            conn.query_row(
                "SELECT flush_phase, flush_phase_updated_at, flush_summary_cache
                 FROM session_memory_state
                 WHERE agent_id = 'agent-1' AND session_id = 'session-1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?;

        assert_eq!(flush_phase, "idle");
        assert_eq!(updated_at, None);
        assert_eq!(summary_cache, None);

        Ok(())
    }

    #[test]
    fn migration_25_adds_created_at_column_for_chunks() -> Result<()> {
        let store = MemoryStore::open_in_memory()?;
        let db = store.db();
        let conn = db.lock().expect("lock");

        let has_created_at: i64 = conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('chunks') WHERE name = 'created_at'",
            [],
            |row| row.get(0),
        )?;

        assert_eq!(has_created_at, 1);
        Ok(())
    }

    #[test]
    fn migration_25_backfills_empty_created_at_from_updated_at_with_legacy_epoch() -> Result<()> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(
            r#"
            CREATE TABLE chunks (
                id TEXT PRIMARY KEY,
                path TEXT NOT NULL,
                source TEXT NOT NULL,
                start_line INTEGER NOT NULL,
                end_line INTEGER NOT NULL,
                hash TEXT NOT NULL,
                model TEXT NOT NULL DEFAULT '',
                text TEXT NOT NULL,
                embedding TEXT NOT NULL DEFAULT '',
                updated_at TEXT NOT NULL,
                access_count INTEGER NOT NULL DEFAULT 0,
                agent_id TEXT NOT NULL DEFAULT '',
                last_accessed TEXT
            );

            CREATE TABLE facts (
                id             TEXT PRIMARY KEY,
                agent_id       TEXT NOT NULL,
                content        TEXT NOT NULL,
                fact_type      TEXT NOT NULL,
                importance     REAL NOT NULL DEFAULT 0.5,
                confidence     REAL NOT NULL DEFAULT 1.0,
                status         TEXT NOT NULL DEFAULT 'active',
                occurred_at    TEXT,
                recorded_at    TEXT NOT NULL,
                source_type    TEXT NOT NULL,
                source_session TEXT,
                access_count   INTEGER NOT NULL DEFAULT 0,
                last_accessed  TEXT,
                superseded_by  TEXT,
                salience       INTEGER NOT NULL DEFAULT 50,
                supersede_reason TEXT,
                created_at     TEXT NOT NULL,
                updated_at     TEXT NOT NULL
            );

            CREATE TABLE session_memory_state (
                agent_id TEXT NOT NULL,
                session_id TEXT NOT NULL,
                session_key TEXT NOT NULL,
                last_flushed_turn INTEGER NOT NULL DEFAULT 0,
                last_boundary_flush_at TEXT,
                pending_flush INTEGER NOT NULL DEFAULT 0,
                recent_explicit_writes TEXT NOT NULL DEFAULT '[]',
                open_episodes TEXT NOT NULL DEFAULT '[]',
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at TEXT NOT NULL DEFAULT (datetime('now')),
                flush_phase TEXT NOT NULL DEFAULT 'idle',
                flush_phase_updated_at TEXT DEFAULT NULL,
                flush_summary_cache TEXT DEFAULT NULL,
                PRIMARY KEY (agent_id, session_id)
            );

            CREATE TABLE embedding_cache (
                provider TEXT NOT NULL,
                model TEXT NOT NULL,
                provider_key TEXT NOT NULL,
                hash TEXT NOT NULL,
                embedding TEXT NOT NULL,
                dims INTEGER NOT NULL,
                updated_at TEXT NOT NULL,
                PRIMARY KEY (provider, model, provider_key, hash)
            );

            CREATE TABLE __schema_version (
                version INTEGER PRIMARY KEY,
                applied_at TEXT NOT NULL DEFAULT (datetime('now'))
            );

            WITH RECURSIVE versions(v) AS (
                SELECT 1
                UNION ALL
                SELECT v + 1 FROM versions WHERE v < 24
            )
            INSERT INTO __schema_version(version, applied_at)
            SELECT v, datetime('now') FROM versions;
            "#,
        )?;

        conn.execute(
            "INSERT INTO chunks(id, path, source, start_line, end_line, hash, model, text, embedding, updated_at, access_count, agent_id, last_accessed)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, '', ?7, '', ?8, 0, ?9, NULL)",
            rusqlite::params![
                "c1",
                "memory/2026-04-04.md",
                "daily",
                1_i64,
                3_i64,
                "h1",
                "hello",
                "1712217600",
                "agent-1"
            ],
        )?;
        conn.execute(
            "INSERT INTO chunks(id, path, source, start_line, end_line, hash, model, text, embedding, updated_at, access_count, agent_id, last_accessed)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, '', ?7, '', ?8, 0, ?9, NULL)",
            rusqlite::params![
                "c2",
                "memory/2026-04-04.md",
                "daily",
                4_i64,
                7_i64,
                "h2",
                "world",
                "",
                "agent-1"
            ],
        )?;

        run_migrations(&conn)?;

        let (created_1, updated_1): (String, String) = conn.query_row(
            "SELECT created_at, updated_at FROM chunks WHERE id = 'c1'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        assert_eq!(created_1, "2024-04-04T08:00:00Z");
        assert_eq!(updated_1, "2024-04-04T08:00:00Z");

        let created_2: String =
            conn.query_row("SELECT created_at FROM chunks WHERE id = 'c2'", [], |row| {
                row.get(0)
            })?;
        assert!(!created_2.is_empty());
        assert!(chrono::DateTime::parse_from_rfc3339(&created_2).is_ok());

        Ok(())
    }

    #[test]
    fn migration_26_adds_affect_columns_with_defaults() -> Result<()> {
        let store = MemoryStore::open_in_memory()?;
        let db = store.db();
        let conn = db.lock().expect("lock");

        conn.execute(
            "INSERT INTO facts(id, agent_id, content, fact_type, importance, confidence, salience, status, occurred_at, recorded_at, source_type, source_session, access_count, last_accessed, superseded_by, supersede_reason, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, 0.8, 1.0, 50, 'active', NULL, ?5, 'test', NULL, 0, NULL, NULL, NULL, ?5, ?5)",
            rusqlite::params![
                "fact-1",
                "agent-1",
                "existing fact",
                "event",
                "2026-04-05T00:00:00Z"
            ],
        )?;

        let (affect, intensity): (String, f64) = conn.query_row(
            "SELECT affect, affect_intensity FROM facts WHERE id = 'fact-1'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;

        assert_eq!(affect, "neutral");
        assert!((intensity - 0.0).abs() < f64::EPSILON);
        Ok(())
    }
}
