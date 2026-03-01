//! Approval registry for coordinating human approval requests between
//! tool executors (requesters) and UI (responders).

use std::collections::HashMap;
use std::sync::Arc;

use clawhive_schema::ApprovalDecision;
use tokio::sync::{oneshot, Mutex};
use uuid::Uuid;

/// A pending approval request.
#[derive(Debug)]
pub struct PendingApproval {
    pub trace_id: Uuid,
    pub command: String,
    pub agent_id: String,
    pub requested_at: chrono::DateTime<chrono::Utc>,
    sender: oneshot::Sender<ApprovalDecision>,
}

/// Registry for managing pending approval requests.
/// Tool executors register requests, UI resolves them.
#[derive(Debug, Clone, Default)]
pub struct ApprovalRegistry {
    pending: Arc<Mutex<HashMap<Uuid, PendingApproval>>>,
    short_id_map: Arc<Mutex<HashMap<String, Uuid>>>,
    runtime_allowlist: Arc<Mutex<HashMap<String, Vec<String>>>>,
}

impl ApprovalRegistry {
    pub fn new() -> Self {
        Self {
            pending: Arc::new(Mutex::new(HashMap::new())),
            short_id_map: Arc::new(Mutex::new(HashMap::new())),
            runtime_allowlist: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Register a new approval request. Returns a receiver that will get the decision.
    pub async fn request(
        &self,
        trace_id: Uuid,
        command: String,
        agent_id: String,
    ) -> oneshot::Receiver<ApprovalDecision> {
        let (tx, rx) = oneshot::channel();
        let approval = PendingApproval {
            trace_id,
            command,
            agent_id,
            requested_at: chrono::Utc::now(),
            sender: tx,
        };
        self.pending.lock().await.insert(trace_id, approval);
        let short_id = trace_id.to_string()[..8].to_string();
        self.short_id_map.lock().await.insert(short_id, trace_id);
        rx
    }

    /// Resolve a pending approval with a decision. Returns Err if trace_id not found.
    pub async fn resolve(&self, trace_id: Uuid, decision: ApprovalDecision) -> Result<(), String> {
        match self.pending.lock().await.remove(&trace_id) {
            Some(approval) => {
                let short_id = trace_id.to_string()[..8].to_string();
                self.short_id_map.lock().await.remove(&short_id);
                let _ = approval.sender.send(decision);
                Ok(())
            }
            None => Err(format!("No pending approval for trace_id {trace_id}")),
        }
    }

    pub async fn resolve_by_short_id(
        &self,
        short_id: &str,
        decision: ApprovalDecision,
    ) -> Result<(), String> {
        let trace_id = {
            let map = self.short_id_map.lock().await;
            map.get(short_id).copied()
        }
        .ok_or_else(|| format!("No pending approval for short id {short_id}"))?;

        self.resolve(trace_id, decision).await
    }

    /// Get a snapshot of all pending approvals (for UI display).
    /// Returns (trace_id, command, agent_id) tuples.
    pub async fn pending_list(&self) -> Vec<(Uuid, String, String)> {
        self.pending
            .lock()
            .await
            .values()
            .map(|a| (a.trace_id, a.command.clone(), a.agent_id.clone()))
            .collect()
    }

    /// Check if there are any pending approvals.
    pub async fn has_pending(&self) -> bool {
        !self.pending.lock().await.is_empty()
    }

    pub async fn add_runtime_allow_pattern(&self, agent_id: &str, pattern: String) {
        let mut map = self.runtime_allowlist.lock().await;
        let entry = map.entry(agent_id.to_string()).or_default();
        if !entry.iter().any(|p| p == &pattern) {
            entry.push(pattern);
        }
    }

    pub async fn is_runtime_allowed(&self, agent_id: &str, command: &str) -> bool {
        let map = self.runtime_allowlist.lock().await;
        let Some(patterns) = map.get(agent_id) else {
            return false;
        };
        patterns
            .iter()
            .any(|pattern| pattern_matches(pattern, command))
    }
}

fn pattern_matches(pattern: &str, command: &str) -> bool {
    let first_token = command.split_whitespace().next().unwrap_or("");
    let basename = std::path::Path::new(first_token)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(first_token);

    if let Some(prefix) = pattern.strip_suffix(" *") {
        basename == prefix || first_token == prefix
    } else {
        command.eq_ignore_ascii_case(pattern) || basename == pattern
    }
}
