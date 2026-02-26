//! Per-session locking to prevent concurrent access to the same session.
//!
//! This ensures that only one request can modify a session's history at a time,
//! preventing race conditions in message ordering and tool execution.

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore};

/// A session lock manager that provides per-session mutual exclusion.
#[derive(Clone)]
pub struct SessionLockManager {
    locks: Arc<Mutex<HashMap<String, Arc<Semaphore>>>>,
    /// Optional global concurrency limit across all sessions
    global_semaphore: Option<Arc<Semaphore>>,
}

impl SessionLockManager {
    /// Create a new session lock manager without global limits.
    pub fn new() -> Self {
        Self {
            locks: Arc::new(Mutex::new(HashMap::new())),
            global_semaphore: None,
        }
    }

    /// Create a new session lock manager with a global concurrency limit.
    pub fn with_global_limit(max_concurrent: usize) -> Self {
        Self {
            locks: Arc::new(Mutex::new(HashMap::new())),
            global_semaphore: Some(Arc::new(Semaphore::new(max_concurrent))),
        }
    }

    /// Acquire exclusive access to a session.
    /// Returns a guard that releases the lock when dropped.
    pub async fn acquire(&self, session_key: &str) -> SessionLockGuard {
        // Acquire global permit first (if configured)
        let global_permit = if let Some(ref sem) = self.global_semaphore {
            Some(sem.clone().acquire_owned().await.expect("semaphore closed"))
        } else {
            None
        };

        // Get or create the per-session semaphore
        let session_sem = {
            let mut locks = self.locks.lock().await;
            locks
                .entry(session_key.to_string())
                .or_insert_with(|| Arc::new(Semaphore::new(1)))
                .clone()
        };

        // Acquire per-session permit
        let session_permit = session_sem.acquire_owned().await.expect("semaphore closed");

        SessionLockGuard {
            _session_permit: session_permit,
            _global_permit: global_permit,
        }
    }

    /// Try to acquire a session lock without blocking.
    /// Returns None if the session is already locked.
    pub async fn try_acquire(&self, session_key: &str) -> Option<SessionLockGuard> {
        // Try global permit first
        let global_permit = if let Some(ref sem) = self.global_semaphore {
            match sem.clone().try_acquire_owned() {
                Ok(permit) => Some(permit),
                Err(_) => return None,
            }
        } else {
            None
        };

        // Get or create the per-session semaphore
        let session_sem = {
            let mut locks = self.locks.lock().await;
            locks
                .entry(session_key.to_string())
                .or_insert_with(|| Arc::new(Semaphore::new(1)))
                .clone()
        };

        // Try to acquire per-session permit
        match session_sem.try_acquire_owned() {
            Ok(permit) => Some(SessionLockGuard {
                _session_permit: permit,
                _global_permit: global_permit,
            }),
            Err(_) => None,
        }
    }

    /// Clean up locks for sessions that are no longer active.
    /// Should be called periodically to prevent memory leaks.
    pub async fn cleanup_unused(&self) {
        let mut locks = self.locks.lock().await;
        locks.retain(|_, sem| {
            // Keep if someone is waiting or holding the lock
            sem.available_permits() < 1
        });
    }
}

impl Default for SessionLockManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Guard that releases the session lock when dropped.
pub struct SessionLockGuard {
    _session_permit: OwnedSemaphorePermit,
    _global_permit: Option<OwnedSemaphorePermit>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[tokio::test]
    async fn test_sequential_access() {
        let manager = SessionLockManager::new();
        let counter = Arc::new(AtomicUsize::new(0));

        let counter1 = counter.clone();
        let manager1 = manager.clone();
        let t1 = tokio::spawn(async move {
            let _guard = manager1.acquire("session1").await;
            counter1.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(50)).await;
            counter1.fetch_add(1, Ordering::SeqCst);
        });

        // Give t1 time to acquire the lock
        tokio::time::sleep(Duration::from_millis(10)).await;

        let counter2 = counter.clone();
        let manager2 = manager.clone();
        let t2 = tokio::spawn(async move {
            let _guard = manager2.acquire("session1").await;
            // Should only run after t1 completes
            assert!(counter2.load(Ordering::SeqCst) >= 2);
            counter2.fetch_add(1, Ordering::SeqCst);
        });

        t1.await.unwrap();
        t2.await.unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn test_different_sessions_parallel() {
        let manager = SessionLockManager::new();
        let counter = Arc::new(AtomicUsize::new(0));

        let counter1 = counter.clone();
        let manager1 = manager.clone();
        let t1 = tokio::spawn(async move {
            let _guard = manager1.acquire("session1").await;
            tokio::time::sleep(Duration::from_millis(50)).await;
            counter1.fetch_add(1, Ordering::SeqCst);
        });

        let counter2 = counter.clone();
        let manager2 = manager.clone();
        let t2 = tokio::spawn(async move {
            let _guard = manager2.acquire("session2").await;
            // Different session, should run in parallel
            counter2.fetch_add(1, Ordering::SeqCst);
        });

        t2.await.unwrap();
        // t2 should complete before t1
        assert_eq!(counter.load(Ordering::SeqCst), 1);
        t1.await.unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn test_global_limit() {
        let manager = SessionLockManager::with_global_limit(2);
        let counter = Arc::new(AtomicUsize::new(0));

        let handles: Vec<_> = (0..5)
            .map(|i| {
                let manager = manager.clone();
                let counter = counter.clone();
                tokio::spawn(async move {
                    let _guard = manager.acquire(&format!("session{i}")).await;
                    let current = counter.fetch_add(1, Ordering::SeqCst);
                    // At most 2 concurrent
                    assert!(current < 2);
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    counter.fetch_sub(1, Ordering::SeqCst);
                })
            })
            .collect();

        for h in handles {
            h.await.unwrap();
        }
    }

    #[tokio::test]
    async fn test_try_acquire() {
        let manager = SessionLockManager::new();

        let guard1 = manager.try_acquire("session1").await;
        assert!(guard1.is_some());

        let guard2 = manager.try_acquire("session1").await;
        assert!(guard2.is_none()); // Already locked

        drop(guard1);

        let guard3 = manager.try_acquire("session1").await;
        assert!(guard3.is_some()); // Now available
    }
}
