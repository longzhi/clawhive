use axum::{extract::State, routing::get, Json, Router};
use clawhive_auth::{AuthProfile, TokenManager};
use serde::Serialize;

use crate::state::AppState;

#[derive(Debug, Serialize)]
pub struct AuthStatusResponse {
    pub active_profile: Option<String>,
    pub profiles: Vec<AuthProfileItem>,
}

#[derive(Debug, Serialize)]
pub struct AuthProfileItem {
    pub name: String,
    pub provider: String,
    pub kind: String,
    pub active: bool,
}

pub fn router() -> Router<AppState> {
    Router::new().route("/status", get(status))
}

async fn status(_state: State<AppState>) -> Json<AuthStatusResponse> {
    let manager = match TokenManager::new() {
        Ok(manager) => manager,
        Err(_) => {
            return Json(AuthStatusResponse {
                active_profile: None,
                profiles: vec![],
            });
        }
    };

    let store = match manager.load_store() {
        Ok(store) => store,
        Err(_) => {
            return Json(AuthStatusResponse {
                active_profile: None,
                profiles: vec![],
            });
        }
    };

    let active = store.active_profile.clone();
    let mut profiles = Vec::with_capacity(store.profiles.len());

    for (name, profile) in store.profiles {
        let (provider, kind) = match profile {
            AuthProfile::ApiKey { provider_id, .. } => (provider_id, "ApiKey".to_string()),
            AuthProfile::OpenAiOAuth { .. } => ("openai".to_string(), "OpenAiOAuth".to_string()),
            AuthProfile::AnthropicSession { .. } => {
                ("anthropic".to_string(), "AnthropicSession".to_string())
            }
        };

        profiles.push(AuthProfileItem {
            active: active.as_deref() == Some(name.as_str()),
            name,
            provider,
            kind,
        });
    }

    Json(AuthStatusResponse {
        active_profile: active,
        profiles,
    })
}
