use std::time::Duration;

pub const DEFAULT_TURN_TIMEOUT_SECS: u64 = 1800;
pub const DEFAULT_TYPING_TTL_SECS: u64 = 120;
pub const DEFAULT_PROGRESS_DELAY_SECS: u64 = 60;
pub const PROGRESS_MESSAGE: &str = "⏳ Still working on it... (send /stop to cancel)";

pub fn default_typing_ttl() -> Duration {
    Duration::from_secs(DEFAULT_TYPING_TTL_SECS.min(DEFAULT_TURN_TIMEOUT_SECS))
}

pub fn default_progress_delay() -> Duration {
    Duration::from_secs(DEFAULT_PROGRESS_DELAY_SECS)
}

pub struct AbortOnDrop(pub tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[cfg(test)]
mod tests {
    use std::future::pending;

    use super::AbortOnDrop;

    #[tokio::test]
    async fn abort_on_drop_aborts_task() {
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let guard = AbortOnDrop(tokio::spawn(async move {
            let _tx = tx;
            pending::<()>().await;
        }));

        drop(guard);

        assert!(rx.await.is_err());
    }
}
