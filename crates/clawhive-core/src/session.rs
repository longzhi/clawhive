use anyhow::Result;
use chrono::{DateTime, Datelike, Local, TimeZone, Timelike, Utc};
use clawhive_memory::MemoryStore;
use clawhive_schema::SessionKey;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionResetPolicy {
    pub idle_minutes: Option<u64>,
    pub daily_at_hour: Option<u8>,
}

impl Default for SessionResetPolicy {
    fn default() -> Self {
        Self {
            idle_minutes: Some(30),
            daily_at_hour: Some(4),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum SessionResetReason {
    Idle,
    Daily,
    Explicit,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub session_key: SessionKey,
    pub session_id: String,
    pub agent_id: String,
    pub created_at: DateTime<Utc>,
    pub last_active: DateTime<Utc>,
    pub ttl_seconds: i64,
    pub interaction_count: u64,
}

impl Session {
    pub fn is_expired(&self) -> bool {
        if self.ttl_seconds <= 0 {
            return false;
        }
        let elapsed = Utc::now() - self.last_active;
        elapsed.num_seconds() >= self.ttl_seconds
    }

    pub fn touch(&mut self) {
        self.last_active = Utc::now();
    }

    pub fn increment_interaction(&mut self) {
        self.interaction_count += 1;
    }
}

pub struct SessionResult {
    pub session: Session,
    pub ended_previous: Option<SessionResetReason>,
    pub previous_session: Option<Session>,
}

#[derive(Clone)]
pub struct SessionManager {
    store: Arc<MemoryStore>,
    default_reset_policy: SessionResetPolicy,
}

impl SessionManager {
    pub fn new(store: Arc<MemoryStore>, default_ttl: i64) -> Self {
        let idle_minutes = if default_ttl >= 0 {
            Some((default_ttl as u64).div_ceil(60))
        } else {
            None
        };
        Self {
            store,
            default_reset_policy: SessionResetPolicy {
                idle_minutes,
                ..SessionResetPolicy::default()
            },
        }
    }

    pub fn with_policy(store: Arc<MemoryStore>, default_reset_policy: SessionResetPolicy) -> Self {
        Self {
            store,
            default_reset_policy,
        }
    }

    pub fn effective_policy(
        &self,
        override_policy: Option<SessionResetPolicy>,
    ) -> SessionResetPolicy {
        override_policy.unwrap_or_else(|| self.default_reset_policy.clone())
    }

    pub async fn get_or_create(&self, key: &SessionKey, agent_id: &str) -> Result<SessionResult> {
        self.get_or_create_with_policy(key, agent_id, None).await
    }

    pub async fn get_or_create_with_policy(
        &self,
        key: &SessionKey,
        agent_id: &str,
        reset_policy: Option<SessionResetPolicy>,
    ) -> Result<SessionResult> {
        let effective_policy = self.effective_policy(reset_policy);
        if let Some(record) = self.store.get_session(&key.0).await? {
            let mut session = Session {
                session_key: key.clone(),
                session_id: if record.session_id.is_empty() {
                    record.session_key.clone()
                } else {
                    record.session_id
                },
                agent_id: record.agent_id,
                created_at: record.created_at,
                last_active: record.last_active,
                ttl_seconds: record.ttl_seconds,
                interaction_count: record.interaction_count,
            };

            if let Some(reason) = self.stale_reason(&session, &effective_policy, Utc::now()) {
                let new_session = self.create_new(key, agent_id, &effective_policy);
                self.persist_session(&new_session).await?;
                return Ok(SessionResult {
                    session: new_session,
                    ended_previous: Some(reason),
                    previous_session: Some(session),
                });
            }

            session.touch();
            self.persist_session(&session).await?;
            Ok(SessionResult {
                session,
                ended_previous: None,
                previous_session: None,
            })
        } else {
            let session = self.create_new(key, agent_id, &effective_policy);
            self.persist_session(&session).await?;
            Ok(SessionResult {
                session,
                ended_previous: None,
                previous_session: None,
            })
        }
    }

    pub async fn get(&self, key: &SessionKey) -> Result<Option<Session>> {
        let Some(record) = self.store.get_session(&key.0).await? else {
            return Ok(None);
        };
        Ok(Some(Session {
            session_key: key.clone(),
            session_id: if record.session_id.is_empty() {
                record.session_key.clone()
            } else {
                record.session_id
            },
            agent_id: record.agent_id,
            created_at: record.created_at,
            last_active: record.last_active,
            ttl_seconds: record.ttl_seconds,
            interaction_count: record.interaction_count,
        }))
    }

    pub async fn reset(&self, key: &SessionKey) -> Result<bool> {
        self.store.delete_session(&key.0).await
    }

    fn create_new(
        &self,
        key: &SessionKey,
        agent_id: &str,
        reset_policy: &SessionResetPolicy,
    ) -> Session {
        let now = Utc::now();
        Session {
            session_key: key.clone(),
            session_id: Uuid::new_v4().to_string(),
            agent_id: agent_id.to_string(),
            created_at: now,
            last_active: now,
            ttl_seconds: reset_policy
                .idle_minutes
                .map(|minutes| (minutes.saturating_mul(60)).min(i64::MAX as u64) as i64)
                .unwrap_or(0),
            interaction_count: 0,
        }
    }

    fn stale_reason(
        &self,
        session: &Session,
        reset_policy: &SessionResetPolicy,
        now: DateTime<Utc>,
    ) -> Option<SessionResetReason> {
        if let Some(idle_minutes) = reset_policy.idle_minutes {
            let idle_seconds = idle_minutes.saturating_mul(60);
            let elapsed_seconds = (now - session.last_active).num_seconds().max(0) as u64;
            if elapsed_seconds >= idle_seconds {
                return Some(SessionResetReason::Idle);
            }
        }

        if let Some(boundary_hour) = reset_policy.daily_at_hour {
            if crossed_daily_reset_boundary(session.last_active, now, boundary_hour, &Local) {
                return Some(SessionResetReason::Daily);
            }
        }

        None
    }

    pub async fn persist_session(&self, session: &Session) -> Result<()> {
        let record = clawhive_memory::SessionRecord {
            session_key: session.session_key.0.clone(),
            session_id: session.session_id.clone(),
            agent_id: session.agent_id.clone(),
            created_at: session.created_at,
            last_active: session.last_active,
            ttl_seconds: session.ttl_seconds,
            interaction_count: session.interaction_count,
        };
        self.store.upsert_session(record).await
    }
}

fn crossed_daily_reset_boundary<Tz>(
    last_active: DateTime<Utc>,
    now: DateTime<Utc>,
    boundary_hour: u8,
    timezone: &Tz,
) -> bool
where
    Tz: TimeZone,
{
    reset_bucket(last_active, boundary_hour, timezone) != reset_bucket(now, boundary_hour, timezone)
}

fn reset_bucket<Tz>(ts: DateTime<Utc>, boundary_hour: u8, timezone: &Tz) -> (i32, u32, u32)
where
    Tz: TimeZone,
{
    let local = ts.with_timezone(timezone);
    let date = local.date_naive();
    let boundary = boundary_hour as u32;
    let bucket_date = if local.hour() < boundary {
        date.pred_opt().unwrap_or(date)
    } else {
        date
    };
    (bucket_date.year(), bucket_date.month(), bucket_date.day())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, FixedOffset, TimeZone};

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
        assert_eq!(result.session.interaction_count, 0);
        assert!(result.ended_previous.is_none());
        assert!(result.previous_session.is_none());
        assert!(!result.session.session_id.is_empty());
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
        assert_eq!(s1.session.session_id, s2.session.session_id);
        assert!(s2.ended_previous.is_none());
        assert!(s2.previous_session.is_none());
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
        assert_eq!(s2.ended_previous, Some(SessionResetReason::Idle));
        assert!(s2.previous_session.is_some());
        assert_ne!(s1.session.session_id, s2.session.session_id,);
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
            session_id: "session-1".to_string(),
            agent_id: "test".to_string(),
            created_at: Utc::now(),
            last_active: Utc::now() - chrono::TimeDelta::try_seconds(100).unwrap(),
            ttl_seconds: 50,
            interaction_count: 0,
        };
        assert!(session.is_expired());
        session.touch();
        assert!(!session.is_expired());
    }

    #[test]
    fn increment_interaction_increases_count() {
        let mut session = Session {
            session_key: test_key(),
            session_id: "session-1".to_string(),
            agent_id: "test".to_string(),
            created_at: Utc::now(),
            last_active: Utc::now(),
            ttl_seconds: 50,
            interaction_count: 0,
        };

        session.increment_interaction();

        assert_eq!(session.interaction_count, 1);
    }

    #[test]
    fn crossed_daily_reset_boundary_detects_local_boundary() {
        let tz = FixedOffset::east_opt(8 * 3600).unwrap();
        let last_active = Utc.with_ymd_and_hms(2026, 3, 30, 19, 30, 0).unwrap(); // 03:30 +08
        let now = Utc.with_ymd_and_hms(2026, 3, 30, 20, 30, 0).unwrap(); // 04:30 +08

        assert!(crossed_daily_reset_boundary(last_active, now, 4, &tz));
    }

    #[tokio::test]
    async fn get_or_create_daily_reset_recreates() {
        let store = Arc::new(MemoryStore::open_in_memory().unwrap());
        let policy = SessionResetPolicy {
            idle_minutes: None,
            daily_at_hour: Some(4),
        };
        let mgr = SessionManager::with_policy(store.clone(), policy);
        let key = test_key();
        let now = Utc::now();
        let last_active = now - Duration::days(1);

        store
            .upsert_session(clawhive_memory::SessionRecord {
                session_key: key.0.clone(),
                session_id: "legacy-session".to_string(),
                agent_id: "clawhive-main".to_string(),
                created_at: now - Duration::days(2),
                last_active,
                ttl_seconds: 0,
                interaction_count: 3,
            })
            .await
            .unwrap();

        let result = mgr.get_or_create(&key, "clawhive-main").await.unwrap();
        assert_eq!(result.ended_previous, Some(SessionResetReason::Daily));
        assert!(result.previous_session.is_some());
    }
}
