use axum::{
    extract::{Path, State},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};

use crate::state::AppState;

#[derive(Serialize)]
pub struct AgentSummary {
    pub agent_id: String,
    pub enabled: bool,
    pub name: String,
    pub emoji: String,
    pub primary_model: String,
    pub tools: Vec<String>,
}

#[derive(Serialize, Deserialize)]
pub struct AgentDetail {
    pub agent_id: String,
    pub enabled: bool,
    pub identity: AgentIdentity,
    pub model_policy: ModelPolicy,
    pub tool_policy: ToolPolicy,
    pub memory_policy: MemoryPolicy,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sub_agent: Option<SubAgentPolicy>,
}

#[derive(Serialize, Deserialize)]
pub struct AgentIdentity {
    pub name: String,
    pub emoji: String,
}

#[derive(Serialize, Deserialize)]
pub struct ModelPolicy {
    pub primary: String,
    pub fallbacks: Vec<String>,
}

#[derive(Serialize, Deserialize)]
pub struct ToolPolicy {
    pub allow: Vec<String>,
}

#[derive(Serialize, Deserialize)]
pub struct MemoryPolicy {
    pub mode: String,
    pub write_scope: String,
}

#[derive(Serialize, Deserialize)]
pub struct SubAgentPolicy {
    pub allow_spawn: bool,
}

#[derive(Deserialize)]
pub struct CreateAgentRequest {
    pub agent_id: String,
    #[serde(default = "default_agent_name")]
    pub name: String,
    #[serde(default = "default_agent_emoji")]
    pub emoji: String,
    pub primary_model: String,
}

fn default_agent_name() -> String {
    "Clawhive".to_string()
}
fn default_agent_emoji() -> String {
    "\u{1F41D}".to_string()
}

#[derive(Serialize)]
pub struct CreateAgentResponse {
    pub agent_id: String,
    pub enabled: bool,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list_agents).post(create_agent))
        .route("/{id}", get(get_agent).put(update_agent))
        .route("/{id}/toggle", post(toggle_agent))
}

async fn list_agents(State(state): State<AppState>) -> Json<Vec<AgentSummary>> {
    let agents_dir = state.root.join("config/agents.d");
    let mut agents = Vec::new();

    if let Ok(entries) = std::fs::read_dir(&agents_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("yaml") {
                continue;
            }
            if let Ok(content) = std::fs::read_to_string(&path) {
                if let Ok(val) = serde_yaml::from_str::<serde_yaml::Value>(&content) {
                    agents.push(AgentSummary {
                        agent_id: val["agent_id"].as_str().unwrap_or("").to_string(),
                        enabled: val["enabled"].as_bool().unwrap_or(false),
                        name: val["identity"]["name"].as_str().unwrap_or("").to_string(),
                        emoji: val["identity"]["emoji"].as_str().unwrap_or("").to_string(),
                        primary_model: val["model_policy"]["primary"]
                            .as_str()
                            .unwrap_or("")
                            .to_string(),
                        tools: val["tool_policy"]["allow"]
                            .as_sequence()
                            .map(|seq| {
                                seq.iter()
                                    .filter_map(|v| v.as_str().map(String::from))
                                    .collect()
                            })
                            .unwrap_or_default(),
                    });
                }
            }
        }
    }

    Json(agents)
}

async fn create_agent(
    State(state): State<AppState>,
    Json(body): Json<CreateAgentRequest>,
) -> Result<Json<CreateAgentResponse>, axum::http::StatusCode> {
    if body.agent_id.trim().is_empty() || body.primary_model.trim().is_empty() {
        return Err(axum::http::StatusCode::BAD_REQUEST);
    }

    let agents_dir = state.root.join("config/agents.d");
    std::fs::create_dir_all(&agents_dir)
        .map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;

    let path = agents_dir.join(format!("{}.yaml", body.agent_id));

    // Write agent YAML (overwrite placeholder if it exists)
    let yaml = format!(
        "agent_id: {}\nenabled: true\nidentity:\n  name: \"{}\"\n  emoji: \"{}\"\nmodel_policy:\n  primary: \"{}\"\n  fallbacks: []\nmemory_policy:\n  mode: \"standard\"\n  write_scope: \"all\"\n",
        body.agent_id, body.name, body.emoji, body.primary_model
    );
    std::fs::write(&path, yaml).map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;

    // Workspace prompt templates (AGENTS.md, SOUL.md, etc.) are created
    // automatically by workspace.init_with_defaults() during agent startup.

    // Update routing.yaml default_agent_id
    let routing_path = state.root.join("config/routing.yaml");
    if let Ok(content) = std::fs::read_to_string(&routing_path) {
        if let Ok(mut val) = serde_yaml::from_str::<serde_yaml::Value>(&content) {
            val["default_agent_id"] = serde_yaml::Value::String(body.agent_id.clone());
            if let Ok(yaml_out) = serde_yaml::to_string(&val) {
                let _ = std::fs::write(&routing_path, yaml_out);
            }
        }
    }

    Ok(Json(CreateAgentResponse {
        agent_id: body.agent_id,
        enabled: true,
    }))
}

async fn get_agent(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<AgentDetail>, axum::http::StatusCode> {
    let path = state.root.join(format!("config/agents.d/{id}.yaml"));
    let content = std::fs::read_to_string(&path).map_err(|_| axum::http::StatusCode::NOT_FOUND)?;
    let detail: AgentDetail = serde_yaml::from_str(&content)
        .map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(detail))
}

async fn update_agent(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(agent): Json<AgentDetail>,
) -> Result<Json<AgentDetail>, axum::http::StatusCode> {
    let path = state.root.join(format!("config/agents.d/{id}.yaml"));

    let content = std::fs::read_to_string(&path).map_err(|_| axum::http::StatusCode::NOT_FOUND)?;
    let update: serde_yaml::Value =
        serde_yaml::to_value(&agent).map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;

    let output = match serde_yaml::from_str::<serde_yaml::Value>(&content) {
        Ok(mut existing) => {
            if let (Some(existing_map), Some(update_map)) =
                (existing.as_mapping_mut(), update.as_mapping())
            {
                for (key, value) in update_map {
                    existing_map.insert(key.clone(), value.clone());
                }
            }
            serde_yaml::to_string(&existing)
                .map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?
        }
        Err(_) => {
            let mut merged = content;
            if !merged.ends_with('\n') {
                merged.push('\n');
            }
            merged.push_str(
                &serde_yaml::to_string(&update)
                    .map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?,
            );
            merged
        }
    };

    std::fs::write(&path, output).map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(agent))
}

#[derive(Serialize)]
struct ToggleResponse {
    agent_id: String,
    enabled: bool,
}

async fn toggle_agent(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<ToggleResponse>, axum::http::StatusCode> {
    let path = state.root.join(format!("config/agents.d/{id}.yaml"));
    let content = std::fs::read_to_string(&path).map_err(|_| axum::http::StatusCode::NOT_FOUND)?;
    let mut val: serde_yaml::Value = serde_yaml::from_str(&content)
        .map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;

    let current = val["enabled"].as_bool().unwrap_or(false);
    val["enabled"] = serde_yaml::Value::Bool(!current);

    let yaml =
        serde_yaml::to_string(&val).map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;
    std::fs::write(&path, yaml).map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(ToggleResponse {
        agent_id: id,
        enabled: !current,
    }))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::{body::Body, http::Request};
    use clawhive_bus::EventBus;
    use tower::ServiceExt;

    use super::router;
    use crate::state::AppState;

    fn write_file(path: &std::path::Path, content: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    fn setup_state() -> (AppState, tempfile::TempDir) {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        (
            AppState {
                root: root.to_path_buf(),
                bus: Arc::new(EventBus::new(16)),
                gateway: None,
                web_password_hash: Arc::new(std::sync::RwLock::new(None)),
                session_store: Arc::new(std::sync::RwLock::new(std::collections::HashMap::new())),
                daemon_mode: false,
                port: 3000,
            },
            tmp,
        )
    }

    #[tokio::test]
    async fn update_agent_preserves_extra_fields() {
        let (state, _tmp) = setup_state();

        // Write agent YAML with extra fields not in AgentDetail
        write_file(
            &state.root.join("config/agents.d/test-agent.yaml"),
            "agent_id: test-agent\nenabled: false\nidentity:\n  name: Old Agent\n  emoji: bee\nmodel_policy:\n  primary: gpt-3.5-turbo\n  fallbacks: []\ntool_policy:\n  allow: []\nmemory_policy:\n  mode: standard\n  write_scope: all\nexec_security: strict\nsandbox:\n  enabled: true\n",
        );

        let app = router().with_state(state.clone());

        let body = serde_json::json!({
            "agent_id": "test-agent",
            "enabled": true,
            "identity": { "name": "Updated Agent", "emoji": "🤖" },
            "model_policy": { "primary": "gpt-4", "fallbacks": [] },
            "tool_policy": { "allow": ["read_file"] },
            "memory_policy": { "mode": "standard", "write_scope": "all" }
        });

        let response = app
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/test-agent")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_string(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), axum::http::StatusCode::OK);

        // Read back the file and verify extra fields are still present
        let yaml =
            std::fs::read_to_string(state.root.join("config/agents.d/test-agent.yaml")).unwrap();

        // AgentDetail fields updated
        assert!(yaml.contains("Updated Agent"), "name should be updated");
        assert!(yaml.contains("gpt-4"), "primary model should be updated");
        assert!(yaml.contains("enabled: true"), "enabled should be updated");

        // Extra fields preserved
        assert!(
            yaml.contains("exec_security: strict"),
            "exec_security must be preserved"
        );
        assert!(
            yaml.contains("enabled: true"),
            "sandbox.enabled must be preserved"
        );
    }
}
