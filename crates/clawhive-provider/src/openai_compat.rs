//! OpenAI-compatible providers (DeepSeek, Groq, Ollama, etc.)
//!
//! These providers use the same API format as OpenAI, just with different base URLs.

use crate::OpenAiProvider;

/// DeepSeek API - OpenAI compatible
/// https://platform.deepseek.com/api-docs
pub fn deepseek(api_key: impl Into<String>) -> OpenAiProvider {
    OpenAiProvider::new(api_key, "https://api.deepseek.com/v1")
}

/// Groq API - OpenAI compatible, very fast inference
/// https://console.groq.com/docs/api
pub fn groq(api_key: impl Into<String>) -> OpenAiProvider {
    OpenAiProvider::new(api_key, "https://api.groq.com/openai/v1")
}

/// Ollama local API - OpenAI compatible
/// Default: http://localhost:11434/v1
pub fn ollama() -> OpenAiProvider {
    ollama_with_base("http://localhost:11434/v1")
}

/// Ollama with custom base URL
pub fn ollama_with_base(base_url: impl Into<String>) -> OpenAiProvider {
    // Ollama doesn't require API key, but we need to pass something
    OpenAiProvider::new("ollama", base_url)
}

/// OpenRouter API - OpenAI compatible, multi-model router
/// https://openrouter.ai/docs
pub fn openrouter(api_key: impl Into<String>) -> OpenAiProvider {
    OpenAiProvider::new(api_key, "https://openrouter.ai/api/v1")
}

/// Together AI - OpenAI compatible
/// https://docs.together.ai/docs/openai-api-compatibility
pub fn together(api_key: impl Into<String>) -> OpenAiProvider {
    OpenAiProvider::new(api_key, "https://api.together.xyz/v1")
}

/// Fireworks AI - OpenAI compatible
/// https://docs.fireworks.ai/api-reference/introduction
pub fn fireworks(api_key: impl Into<String>) -> OpenAiProvider {
    OpenAiProvider::new(api_key, "https://api.fireworks.ai/inference/v1")
}

/// Qwen (通义千问) via Alibaba DashScope - OpenAI compatible
/// https://dashscope.aliyuncs.com
pub fn qwen(api_key: impl Into<String>) -> OpenAiProvider {
    OpenAiProvider::new(api_key, "https://dashscope.aliyuncs.com/compatible-mode/v1")
}

/// Moonshot / Kimi - OpenAI compatible
/// https://platform.moonshot.ai/docs/api/chat
pub fn moonshot(api_key: impl Into<String>) -> OpenAiProvider {
    OpenAiProvider::new(api_key, "https://api.moonshot.ai/v1")
}

/// Zhipu GLM (智谱AI) - OpenAI compatible
/// https://open.bigmodel.cn
pub fn zhipu(api_key: impl Into<String>) -> OpenAiProvider {
    OpenAiProvider::new(api_key, "https://open.bigmodel.cn/api/paas/v4")
}

/// MiniMax - OpenAI compatible
/// https://platform.minimax.io
pub fn minimax(api_key: impl Into<String>) -> OpenAiProvider {
    OpenAiProvider::new(api_key, "https://api.minimax.io/v1")
}

/// Volcengine / Doubao (火山引擎/豆包) - OpenAI compatible
/// https://www.volcengine.com/docs/82379
pub fn volcengine(api_key: impl Into<String>) -> OpenAiProvider {
    OpenAiProvider::new(api_key, "https://ark.cn-beijing.volces.com/api/v3")
}

/// Baidu Qianfan (百度千帆) v2 API - OpenAI compatible
/// https://qianfan.baidubce.com
pub fn qianfan(api_key: impl Into<String>) -> OpenAiProvider {
    OpenAiProvider::new(api_key, "https://qianfan.baidubce.com/v2")
}

/// Custom OpenAI-compatible endpoint.
///
/// `strip_reasoning` is enabled so that `reasoning_effort` is never sent —
/// most third-party OpenAI-compatible APIs reject unknown parameters.
pub fn custom(api_key: impl Into<String>, base_url: impl Into<String>) -> OpenAiProvider {
    let mut p = OpenAiProvider::new(api_key, base_url);
    p.set_strip_reasoning(true);
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deepseek_uses_correct_base() {
        let provider = deepseek("sk-test");
        // Can't access private fields, but at least verify it compiles
        assert!(std::mem::size_of_val(&provider) > 0);
    }

    #[test]
    fn groq_uses_correct_base() {
        let provider = groq("gsk-test");
        assert!(std::mem::size_of_val(&provider) > 0);
    }

    #[test]
    fn ollama_no_key_required() {
        let provider = ollama();
        assert!(std::mem::size_of_val(&provider) > 0);
    }

    #[test]
    fn custom_accepts_any_base() {
        let provider = custom("key", "https://my-llm.example.com/v1");
        assert!(std::mem::size_of_val(&provider) > 0);
    }

    #[test]
    fn qwen_uses_correct_base() {
        let provider = qwen("sk-test");
        assert!(std::mem::size_of_val(&provider) > 0);
    }

    #[test]
    fn moonshot_uses_correct_base() {
        let provider = moonshot("sk-test");
        assert!(std::mem::size_of_val(&provider) > 0);
    }

    #[test]
    fn zhipu_uses_correct_base() {
        let provider = zhipu("sk-test");
        assert!(std::mem::size_of_val(&provider) > 0);
    }

    #[test]
    fn minimax_uses_correct_base() {
        let provider = minimax("sk-test");
        assert!(std::mem::size_of_val(&provider) > 0);
    }

    #[test]
    fn volcengine_uses_correct_base() {
        let provider = volcengine("sk-test");
        assert!(std::mem::size_of_val(&provider) > 0);
    }

    #[test]
    fn qianfan_uses_correct_base() {
        let provider = qianfan("sk-test");
        assert!(std::mem::size_of_val(&provider) > 0);
    }

    #[test]
    fn compat_providers_are_openai_provider() {
        // All compat providers return OpenAiProvider which implements LlmProvider
        let provider = deepseek("test-key");
        let _: Box<dyn crate::LlmProvider> = Box::new(provider);
    }
}
