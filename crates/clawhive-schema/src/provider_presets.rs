use serde::Serialize;

/// Metadata for a known model within a provider preset.
#[derive(Debug, Clone, Serialize)]
pub struct ModelPresetInfo {
    pub id: &'static str,
    /// Context window size in tokens.
    pub context_window: u32,
    /// Maximum output tokens the model can generate.
    pub max_output_tokens: u32,
    /// Whether this is a reasoning/thinking model (o3, deepseek-reasoner, etc.)
    pub reasoning: bool,
    /// Whether the model supports image/vision input.
    pub vision: bool,
}

/// A known LLM provider preset.
#[derive(Debug, Clone, Serialize)]
pub struct ProviderPreset {
    pub id: &'static str,
    pub name: &'static str,
    pub api_base: &'static str,
    pub needs_key: bool,
    pub needs_base_url: bool,
    pub default_model: &'static str,
    pub models: &'static [ModelPresetInfo],
}

const fn m(
    id: &'static str,
    context_window: u32,
    max_output_tokens: u32,
    reasoning: bool,
    vision: bool,
) -> ModelPresetInfo {
    ModelPresetInfo {
        id,
        context_window,
        max_output_tokens,
        reasoning,
        vision,
    }
}

pub const PROVIDER_PRESETS: &[ProviderPreset] = &[
    ProviderPreset {
        id: "anthropic",
        name: "Anthropic",
        api_base: "https://api.anthropic.com/v1",
        needs_key: true,
        needs_base_url: false,
        default_model: "claude-opus-4-6",
        models: &[
            m("claude-opus-4-6", 200_000, 32768, false, true),
            m("claude-sonnet-4-6", 200_000, 16384, false, true),
            m("claude-opus-4-5", 200_000, 32768, false, true),
            m("claude-sonnet-4-5", 200_000, 16384, false, true),
            m("claude-haiku-4-5", 200_000, 8192, false, true),
        ],
    },
    ProviderPreset {
        id: "openai",
        name: "OpenAI",
        api_base: "https://api.openai.com/v1",
        needs_key: true,
        needs_base_url: false,
        default_model: "gpt-5.2",
        models: &[
            m("gpt-5.2", 200_000, 16384, false, true),
            m("gpt-5.2-pro", 200_000, 32768, false, true),
            m("gpt-5", 128_000, 16384, false, true),
            m("gpt-5-pro", 128_000, 32768, false, true),
            m("gpt-5-mini", 128_000, 16384, false, true),
            m("o3-pro", 200_000, 100_000, true, true),
        ],
    },
    ProviderPreset {
        id: "openai-chatgpt",
        name: "OpenAI OAuth",
        api_base: "https://chatgpt.com/backend-api/codex",
        needs_key: false,
        needs_base_url: false,
        default_model: "gpt-5.3-codex",
        models: &[m("gpt-5.3-codex", 200_000, 16384, false, false)],
    },
    ProviderPreset {
        id: "azure-openai",
        name: "Azure OpenAI",
        api_base: "https://<your-resource>.openai.azure.com/openai/v1",
        needs_key: true,
        needs_base_url: true,
        default_model: "gpt-5.3-codex",
        models: &[
            m("gpt-5.3-codex", 200_000, 16384, false, false),
            m("gpt-5.2", 200_000, 16384, false, true),
            m("gpt-5.2-codex", 200_000, 16384, false, false),
            m("gpt-5.1-codex-max", 200_000, 32768, false, false),
            m("o3-pro", 200_000, 100_000, true, true),
        ],
    },
    ProviderPreset {
        id: "gemini",
        name: "Google Gemini",
        api_base: "https://generativelanguage.googleapis.com/v1beta",
        needs_key: true,
        needs_base_url: false,
        default_model: "gemini-2.5-pro",
        models: &[
            m("gemini-2.5-pro", 1_000_000, 65536, false, true),
            m("gemini-2.5-flash", 1_000_000, 65536, false, true),
            m("gemini-2.0-flash", 1_000_000, 8192, false, true),
        ],
    },
    ProviderPreset {
        id: "deepseek",
        name: "DeepSeek",
        api_base: "https://api.deepseek.com/v1",
        needs_key: true,
        needs_base_url: false,
        default_model: "deepseek-chat",
        models: &[
            m("deepseek-chat", 65_536, 8192, false, false),
            m("deepseek-reasoner", 65_536, 8192, true, false),
        ],
    },
    ProviderPreset {
        id: "groq",
        name: "Groq",
        api_base: "https://api.groq.com/openai/v1",
        needs_key: true,
        needs_base_url: false,
        default_model: "llama-3.3-70b-versatile",
        models: &[
            m("llama-3.3-70b-versatile", 128_000, 32768, false, false),
            m("llama-3.1-8b-instant", 128_000, 8192, false, false),
        ],
    },
    ProviderPreset {
        id: "ollama",
        name: "Ollama",
        api_base: "http://localhost:11434/v1",
        needs_key: false,
        needs_base_url: false,
        default_model: "llama3.2",
        models: &[
            m("llama3.2", 128_000, 8192, false, false),
            m("qwen2.5-coder", 32_768, 8192, false, false),
            m("mistral", 32_768, 8192, false, false),
        ],
    },
    ProviderPreset {
        id: "openrouter",
        name: "OpenRouter",
        api_base: "https://openrouter.ai/api/v1",
        needs_key: true,
        needs_base_url: false,
        default_model: "anthropic/claude-sonnet-4-6",
        models: &[
            m("openai/gpt-5.3-codex", 200_000, 16384, false, false),
            m("anthropic/claude-opus-4-6", 200_000, 32768, false, true),
            m("google/gemini-2.5-pro", 1_000_000, 65536, false, true),
            m("openai/gpt-5.2", 200_000, 16384, false, true),
        ],
    },
    ProviderPreset {
        id: "together",
        name: "Together AI",
        api_base: "https://api.together.xyz/v1",
        needs_key: true,
        needs_base_url: false,
        default_model: "meta-llama/Llama-3.3-70B-Instruct-Turbo",
        models: &[
            m(
                "meta-llama/Llama-3.3-70B-Instruct-Turbo",
                128_000,
                8192,
                false,
                false,
            ),
            m(
                "meta-llama/Llama-4-Scout-17B-16E-Instruct",
                512_000,
                8192,
                false,
                true,
            ),
        ],
    },
    ProviderPreset {
        id: "fireworks",
        name: "Fireworks AI",
        api_base: "https://api.fireworks.ai/inference/v1",
        needs_key: true,
        needs_base_url: false,
        default_model: "accounts/fireworks/models/llama-v3p3-70b-instruct",
        models: &[
            m(
                "accounts/fireworks/models/llama-v3p3-70b-instruct",
                128_000,
                8192,
                false,
                false,
            ),
            m(
                "accounts/fireworks/models/llama4-scout-instruct-basic",
                128_000,
                8192,
                false,
                true,
            ),
        ],
    },
    ProviderPreset {
        id: "qwen",
        name: "Qwen (Alibaba)",
        api_base: "https://dashscope.aliyuncs.com/compatible-mode/v1",
        needs_key: true,
        needs_base_url: false,
        default_model: "qwen-max",
        models: &[
            m("qwen-max", 32_768, 8192, false, false),
            m("qwen-plus", 131_072, 8192, false, false),
            m("qwen-turbo", 131_072, 8192, false, false),
            m("qwen-long", 1_000_000, 8192, false, false),
        ],
    },
    ProviderPreset {
        id: "moonshot",
        name: "Moonshot AI",
        api_base: "https://api.moonshot.cn/v1",
        needs_key: true,
        needs_base_url: false,
        default_model: "moonshot-v1-128k",
        models: &[
            m("moonshot-v1-128k", 128_000, 8192, false, false),
            m("moonshot-v1-32k", 32_768, 8192, false, false),
            m("moonshot-v1-8k", 8_192, 4096, false, false),
        ],
    },
    ProviderPreset {
        id: "zhipu",
        name: "Zhipu AI",
        api_base: "https://open.bigmodel.cn/api/paas/v4",
        needs_key: true,
        needs_base_url: false,
        default_model: "glm-4-plus",
        models: &[
            m("glm-4-plus", 128_000, 4096, false, true),
            m("glm-4-flash", 128_000, 4096, false, true),
            m("glm-4-long", 1_000_000, 4096, false, false),
            m("glm-4", 128_000, 4096, false, true),
        ],
    },
    ProviderPreset {
        id: "minimax",
        name: "MiniMax",
        api_base: "https://api.minimax.chat/v1",
        needs_key: true,
        needs_base_url: false,
        default_model: "MiniMax-Text-01",
        models: &[
            m("MiniMax-Text-01", 1_000_000, 8192, false, false),
            m("abab6.5s-chat", 245_760, 8192, false, false),
        ],
    },
    ProviderPreset {
        id: "volcengine",
        name: "Volcengine (Doubao)",
        api_base: "https://ark.cn-beijing.volces.com/api/v3",
        needs_key: true,
        needs_base_url: false,
        default_model: "doubao-pro-128k",
        models: &[
            m("doubao-pro-128k", 128_000, 4096, false, false),
            m("doubao-pro-32k", 32_768, 4096, false, false),
            m("doubao-lite-128k", 128_000, 4096, false, false),
        ],
    },
    ProviderPreset {
        id: "qianfan",
        name: "Baidu Qianfan",
        api_base: "https://qianfan.baidubce.com/v2",
        needs_key: true,
        needs_base_url: false,
        default_model: "ernie-4.0-8k",
        models: &[
            m("ernie-4.0-8k", 8_192, 4096, false, false),
            m("ernie-4.0-turbo-8k", 8_192, 4096, false, false),
            m("ernie-3.5-8k", 8_192, 4096, false, false),
        ],
    },
];

/// Look up a provider preset by id.
pub fn preset_by_id(id: &str) -> Option<&'static ProviderPreset> {
    PROVIDER_PRESETS.iter().find(|p| p.id == id)
}

/// Look up model metadata by provider id and model id.
pub fn model_info(provider_id: &str, model_id: &str) -> Option<&'static ModelPresetInfo> {
    preset_by_id(provider_id).and_then(|p| p.models.iter().find(|m| m.id == model_id))
}

/// Get model list for a provider id (with `provider_id/` prefix).
pub fn provider_models_for_id(provider_id: &str) -> Vec<String> {
    match preset_by_id(provider_id) {
        Some(p) => p
            .models
            .iter()
            .map(|m| format!("{}/{}", provider_id, m.id))
            .collect(),
        None => vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::{model_info, provider_models_for_id};

    #[test]
    fn provider_models_for_id_returns_fully_qualified_ids() {
        let models = provider_models_for_id("openai");
        assert_eq!(
            models,
            vec![
                "openai/gpt-5.2",
                "openai/gpt-5.2-pro",
                "openai/gpt-5",
                "openai/gpt-5-pro",
                "openai/gpt-5-mini",
                "openai/o3-pro",
            ]
        );
    }

    #[test]
    fn model_info_returns_metadata_for_known_model() {
        let info = model_info("azure-openai", "gpt-5.3-codex").expect("model should exist");
        assert_eq!(info.context_window, 200_000);
        assert_eq!(info.max_output_tokens, 16384);
        assert!(!info.reasoning);
        assert!(!info.vision);
    }

    #[test]
    fn model_info_returns_none_for_unknown_model() {
        assert!(model_info("openai", "not-a-model").is_none());
        assert!(model_info("not-a-provider", "gpt-5.2").is_none());
    }

    #[test]
    fn provider_models_for_openai_chatgpt_returns_codex_model() {
        let models = provider_models_for_id("openai-chatgpt");
        assert_eq!(models, vec!["openai-chatgpt/gpt-5.3-codex"]);
    }
}
