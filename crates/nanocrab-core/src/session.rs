use anyhow::Result;
use chrono::{DateTime, Utc};
use nanocrab_memory::MemoryStore;
use nanocrab_schema::SessionKey;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub session_key: SessionKey,
    pub agent_id: String,
    pub created_at: DateTime<Utc>,
    pub last_active: DateTime<Utc>,
    pub ttl_seconds: i64,
}

impl Session {
    pub fn is_expired(&self) -> bool {
        let elapsed = Utc::now() - self.last_active;
        elapsed.num_seconds() >= self.ttl_seconds
    }

    pub fn touch(&mut self) {
        self.last_active = Utc::now();
    }
}

pub struct SessionManager {
    store: Arc<MemoryStore>,
    default_ttl: i64,
}

impl SessionManager {
    pub fn new(store: Arc<MemoryStore>, default_ttl: i64) -> Self {
        Self { store, default_ttl }
    }

    pub async fn get_or_create(&self, key: &SessionKey, agent_id: &str) -> Result<Session> {
        if let Some(record) = self.store.get_session(&key.0).await? {
            let mut session = Session {
                session_key: key.clone(),
                agent_id: record.agent_id,
                created_at: record.created_at,
                last_active: record.last_active,
                ttl_seconds: record.ttl_seconds,
            };

            if session.is_expired() {
                let new_session = self.create_new(key, agent_id);
                self.persist(&new_session).await?;
                return Ok(new_session);
            }

            session.touch();
            self.persist(&session).await?;
            Ok(session)
        } else {
            let session = self.create_new(key, agent_id);
            self.persist(&session).await?;
            Ok(session)
        }
    }

    fn create_new(&self, key: &SessionKey, agent_id: &str) -> Session {
        let now = Utc::now();
        Session {
            session_key: key.clone(),
            agent_id: agent_id.to_string(),
            created_at: now,
            last_active: now,
            ttl_seconds: self.default_ttl,
        }
    }

    async fn persist(&self, session: &Session) -> Result<()> {
        let record = nanocrab_memory::SessionRecord {
            session_key: session.session_key.0.clone(),
            agent_id: session.agent_id.clone(),
            created_at: session.created_at,
            last_active: session.last_active,
            ttl_seconds: session.ttl_seconds,
        };
        self.store.upsert_session(record).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_key() -> SessionKey {
        SessionKey("telegram:tg_main:chat:1:user:1".to_string())
    }

    #[tokio::test]
    async fn get_or_create_new_session() {
        let store = Arc::new(MemoryStore::open_in_memory().unwrap());
        let mgr = SessionManager::new(store, 1800);
        let session = mgr
            .get_or_create(&test_key(), "nanocrab-main")
            .await
            .unwrap();
        assert_eq!(session.agent_id, "nanocrab-main");
        assert_eq!(session.ttl_seconds, 1800);
    }

    #[tokio::test]
    async fn get_or_create_reuses_existing() {
        let store = Arc::new(MemoryStore::open_in_memory().unwrap());
        let mgr = SessionManager::new(store, 1800);
        let s1 = mgr
            .get_or_create(&test_key(), "nanocrab-main")
            .await
            .unwrap();
        let s2 = mgr
            .get_or_create(&test_key(), "nanocrab-main")
            .await
            .unwrap();
        assert_eq!(s1.created_at, s2.created_at);
    }

    #[tokio::test]
    async fn get_or_create_expired_recreates() {
        let store = Arc::new(MemoryStore::open_in_memory().unwrap());
        let mgr = SessionManager::new(store, 0);
        let s1 = mgr
            .get_or_create(&test_key(), "nanocrab-main")
            .await
            .unwrap();
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        let s2 = mgr
            .get_or_create(&test_key(), "nanocrab-main")
            .await
            .unwrap();
        assert_ne!(s1.created_at, s2.created_at);
    }

    #[test]
    fn session_is_expired() {
        let mut session = Session {
            session_key: test_key(),
            agent_id: "test".to_string(),
            created_at: Utc::now(),
            last_active: Utc::now() - chrono::TimeDelta::try_seconds(100).unwrap(),
            ttl_seconds: 50,
        };
        assert!(session.is_expired());
        session.touch();
        assert!(!session.is_expired());
    }
}
