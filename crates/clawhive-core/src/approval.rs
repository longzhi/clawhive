//! Approval registry for coordinating human approval requests between
//! tool executors (requesters) and UI (responders).

use std::collections::HashMap;
use std::path::PathBuf;
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

/// Persisted runtime allowlist — survives process restarts.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
struct PersistedAllowlist {
    agents: HashMap<String, AgentAllowlist>,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
struct AgentAllowlist {
    #[serde(default)]
    exec: Vec<String>,
    #[serde(default)]
    network: Vec<String>,
}

#[derive(serde::Deserialize)]
struct LegacyAllowlist {
    agents: HashMap<String, Vec<String>>,
}

/// Registry for managing pending approval requests.
/// Tool executors register requests, UI resolves them.
#[derive(Debug, Clone)]
pub struct ApprovalRegistry {
    pending: Arc<Mutex<HashMap<Uuid, PendingApproval>>>,
    short_id_map: Arc<Mutex<HashMap<String, Uuid>>>,
    runtime_allowlist: Arc<Mutex<HashMap<String, AgentAllowlist>>>,
    /// Path to persist runtime allowlist (None = in-memory only, for tests)
    persist_path: Option<PathBuf>,
}

impl Default for ApprovalRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ApprovalRegistry {
    pub fn new() -> Self {
        Self {
            pending: Arc::new(Mutex::new(HashMap::new())),
            short_id_map: Arc::new(Mutex::new(HashMap::new())),
            runtime_allowlist: Arc::new(Mutex::new(HashMap::new())),
            persist_path: None,
        }
    }

    /// Create a registry that persists the runtime allowlist to disk.
    pub fn with_persistence(path: PathBuf) -> Self {
        // Load existing allowlist from disk
        let loaded = if path.exists() {
            match std::fs::read_to_string(&path) {
                Ok(data) => match serde_json::from_str::<PersistedAllowlist>(&data) {
                    Ok(new_format) => new_format.agents,
                    Err(_) => match serde_json::from_str::<LegacyAllowlist>(&data) {
                        Ok(legacy) => legacy
                            .agents
                            .into_iter()
                            .map(|(agent_id, exec)| {
                                (
                                    agent_id,
                                    AgentAllowlist {
                                        exec,
                                        network: Vec::new(),
                                    },
                                )
                            })
                            .collect(),
                        Err(_) => HashMap::new(),
                    },
                },
                Err(_) => HashMap::new(),
            }
        } else {
            HashMap::new()
        };

        Self {
            pending: Arc::new(Mutex::new(HashMap::new())),
            short_id_map: Arc::new(Mutex::new(HashMap::new())),
            runtime_allowlist: Arc::new(Mutex::new(loaded)),
            persist_path: Some(path),
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
        if !entry.exec.iter().any(|p| p == &pattern) {
            entry.exec.push(pattern);
        }
        self.persist(&map);
    }

    pub async fn is_runtime_allowed(&self, agent_id: &str, command: &str) -> bool {
        let map = self.runtime_allowlist.lock().await;
        let Some(agent) = map.get(agent_id) else {
            return false;
        };
        agent
            .exec
            .iter()
            .any(|pattern| pattern_matches(pattern, command))
    }

    pub async fn add_network_allow_pattern(&self, agent_id: &str, pattern: String) {
        let mut map = self.runtime_allowlist.lock().await;
        let entry = map.entry(agent_id.to_string()).or_default();
        if !entry.network.iter().any(|p| p == &pattern) {
            entry.network.push(pattern);
        }
        self.persist(&map);
    }

    pub async fn is_network_allowed(&self, agent_id: &str, host: &str, port: u16) -> bool {
        let map = self.runtime_allowlist.lock().await;
        let Some(agent) = map.get(agent_id) else {
            return false;
        };
        let target = format!("{host}:{port}");
        agent
            .network
            .iter()
            .any(|pattern| network_pattern_matches(pattern, &target))
    }

    fn persist(&self, map: &HashMap<String, AgentAllowlist>) {
        if let Some(ref path) = self.persist_path {
            let persisted = PersistedAllowlist {
                agents: map.clone(),
            };
            if let Ok(data) = serde_json::to_string_pretty(&persisted) {
                if let Some(parent) = path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let _ = std::fs::write(path, data);
            }
        }
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

fn network_pattern_matches(pattern: &str, target: &str) -> bool {
    let Some((pat_host, pat_port)) = pattern.rsplit_once(':') else {
        return pattern == target;
    };
    let Some((tgt_host, _tgt_port)) = target.rsplit_once(':') else {
        return false;
    };
    if pat_host != tgt_host {
        return false;
    }
    pat_port == "*" || pattern == target
}
