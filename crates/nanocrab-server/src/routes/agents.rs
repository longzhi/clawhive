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

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list_agents))
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

async fn get_agent(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<AgentDetail>, axum::http::StatusCode> {
    let path = state.root.join(format!("config/agents.d/{id}.yaml"));
    let content = std::fs::read_to_string(&path).map_err(|_| axum::http::StatusCode::NOT_FOUND)?;
    let detail: AgentDetail =
        serde_yaml::from_str(&content).map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(detail))
}

async fn update_agent(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(agent): Json<AgentDetail>,
) -> Result<Json<AgentDetail>, axum::http::StatusCode> {
    let path = state.root.join(format!("config/agents.d/{id}.yaml"));
    let yaml =
        serde_yaml::to_string(&agent).map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;
    std::fs::write(&path, yaml).map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;
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
    let mut val: serde_yaml::Value =
        serde_yaml::from_str(&content).map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;

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
