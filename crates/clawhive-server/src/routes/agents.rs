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

    // Create default system prompt if it doesn't exist
    let prompts_dir = state.root.join(format!("prompts/{}", body.agent_id));
    std::fs::create_dir_all(&prompts_dir)
        .map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;
    let system_md = prompts_dir.join("system.md");
    if !system_md.exists() {
        let prompt = format!(
            "You are {}, a helpful AI assistant powered by clawhive.\n\nYou are knowledgeable, concise, and friendly. When you don't know something, you say so honestly.\n",
            body.name
        );
        std::fs::write(&system_md, prompt)
            .map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;
    }

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
