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
