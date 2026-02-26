use anyhow::Result;
use chrono::{DateTime, Utc};
use clawhive_memory::MemoryStore;
use clawhive_schema::SessionKey;
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

/// Result of get_or_create: the session plus whether it was freshly created
/// from an expired session (indicating fallback summary may be needed).
pub struct SessionResult {
    pub session: Session,
    /// If this session replaced an expired one, contains the old session's key.
    /// Used to trigger fallback summary generation.
    pub expired_previous: bool,
}

pub struct SessionManager {
    store: Arc<MemoryStore>,
    default_ttl: i64,
}

impl SessionManager {
    pub fn new(store: Arc<MemoryStore>, default_ttl: i64) -> Self {
        Self { store, default_ttl }
    }

    pub async fn get_or_create(&self, key: &SessionKey, agent_id: &str) -> Result<SessionResult> {
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
                return Ok(SessionResult {
                    session: new_session,
                    expired_previous: true,
                });
            }

            session.touch();
            self.persist(&session).await?;
            Ok(SessionResult {
                session,
                expired_previous: false,
            })
        } else {
            let session = self.create_new(key, agent_id);
            self.persist(&session).await?;
            Ok(SessionResult {
                session,
                expired_previous: false,
            })
        }
    }

    pub async fn reset(&self, key: &SessionKey) -> Result<bool> {
        self.store.delete_session(&key.0).await
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
        let record = clawhive_memory::SessionRecord {
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
        let result = mgr
            .get_or_create(&test_key(), "clawhive-main")
            .await
            .unwrap();
        assert_eq!(result.session.agent_id, "clawhive-main");
        assert_eq!(result.session.ttl_seconds, 1800);
        assert!(!result.expired_previous);
    }

    #[tokio::test]
    async fn get_or_create_reuses_existing() {
        let store = Arc::new(MemoryStore::open_in_memory().unwrap());
        let mgr = SessionManager::new(store, 1800);
        let s1 = mgr
            .get_or_create(&test_key(), "clawhive-main")
            .await
            .unwrap();
        let s2 = mgr
            .get_or_create(&test_key(), "clawhive-main")
            .await
            .unwrap();
        assert_eq!(s1.session.created_at, s2.session.created_at);
        assert!(!s2.expired_previous);
    }

    #[tokio::test]
    async fn get_or_create_expired_recreates() {
        let store = Arc::new(MemoryStore::open_in_memory().unwrap());
        let mgr = SessionManager::new(store, 0);
        let s1 = mgr
            .get_or_create(&test_key(), "clawhive-main")
            .await
            .unwrap();
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        let s2 = mgr
            .get_or_create(&test_key(), "clawhive-main")
            .await
            .unwrap();
        assert_ne!(s1.session.created_at, s2.session.created_at);
        assert!(s2.expired_previous);
    }

    #[tokio::test]
    async fn reset_deletes_existing_session() {
        let store = Arc::new(MemoryStore::open_in_memory().unwrap());
        let mgr = SessionManager::new(store.clone(), 1800);
        let key = test_key();

        mgr.get_or_create(&key, "clawhive-main").await.unwrap();
        let deleted = mgr.reset(&key).await.unwrap();
        assert!(deleted);
        let loaded = store.get_session(&key.0).await.unwrap();
        assert!(loaded.is_none());
    }

    #[tokio::test]
    async fn reset_returns_false_for_missing_session() {
        let store = Arc::new(MemoryStore::open_in_memory().unwrap());
        let mgr = SessionManager::new(store, 1800);
        let deleted = mgr.reset(&test_key()).await.unwrap();
        assert!(!deleted);
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
