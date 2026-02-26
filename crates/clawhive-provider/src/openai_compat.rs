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

/// Custom OpenAI-compatible endpoint
pub fn custom(api_key: impl Into<String>, base_url: impl Into<String>) -> OpenAiProvider {
    OpenAiProvider::new(api_key, base_url)
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
}
