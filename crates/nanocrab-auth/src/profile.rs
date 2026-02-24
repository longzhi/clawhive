use std::collections::HashMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
#[serde(tag = "type")]
pub enum AuthProfile {
    ApiKey {
        provider_id: String,
        api_key: String,
    },
    OpenAiOAuth {
        access_token: String,
        refresh_token: String,
        expires_at: i64,
        #[serde(default)]
        chatgpt_account_id: Option<String>,
    },
    AnthropicSession {
        session_token: String,
    },
}

#[derive(Debug, Default, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct AuthStore {
    pub active_profile: Option<String>,
    pub profiles: HashMap<String, AuthProfile>,
}


#[cfg(test)]
mod tests {
    use super::{AuthProfile, AuthStore};
    use std::collections::HashMap;

    #[test]
    fn auth_store_roundtrips_json() {
        let mut profiles = HashMap::new();
        profiles.insert(
            "openai-main".to_string(),
            AuthProfile::OpenAiOAuth {
                access_token: "at_123".to_string(),
                refresh_token: "rt_456".to_string(),
                expires_at: 1_750_000_000,
                chatgpt_account_id: Some("acct_123".to_string()),
            },
        );
        profiles.insert(
            "anthropic-main".to_string(),
            AuthProfile::AnthropicSession {
                session_token: "st_abc".to_string(),
            },
        );

        let store = AuthStore {
            active_profile: Some("openai-main".to_string()),
            profiles,
        };

        let serialized = serde_json::to_string_pretty(&store).expect("serialize auth store");
        let parsed: AuthStore = serde_json::from_str(&serialized).expect("deserialize auth store");

        assert_eq!(parsed, store);
    }

    #[test]
    fn auth_profile_uses_tagged_type_field() {
        let profile = AuthProfile::ApiKey {
            provider_id: "openai".to_string(),
            api_key: "sk-test".to_string(),
        };

        let value = serde_json::to_value(profile).expect("serialize profile");
        assert_eq!(value["type"], "ApiKey");
        assert_eq!(value["provider_id"], "openai");
        assert_eq!(value["api_key"], "sk-test");
    }
}
