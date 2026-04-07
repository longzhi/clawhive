/// Typed errors for the provider crate's public API.
///
/// Currently covers registry/config operations (`create_provider`,
/// `register_from_configs`, `ProviderRegistry::get`).
///
/// `LlmProvider::chat()` still returns `anyhow::Result` — migrating the
/// trait signature requires updating every provider impl atomically and
/// is planned for a later phase.
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    /// The requested provider ID was not found in the registry.
    #[error("provider not found: {0}")]
    NotFound(String),

    /// A required configuration field is missing (e.g. `api_key`).
    #[error("missing required config for {provider}: {field}")]
    MissingConfig {
        provider: String,
        field: &'static str,
    },

    /// Authentication failed (bad key, expired token, etc.).
    #[error("authentication failed: {0}")]
    AuthFailed(String),

    /// The provider returned an HTTP-level error.
    #[error("API error ({status}): {message}")]
    ApiError { status: u16, message: String },

    /// The provider indicated rate limiting.
    #[error("rate limited: retry after {retry_after_ms}ms")]
    RateLimited { retry_after_ms: u64 },

    /// Request timed out.
    #[error("request timeout")]
    Timeout,

    /// The response could not be parsed or was otherwise invalid.
    #[error("invalid response: {0}")]
    InvalidResponse(String),

    /// Catch-all for errors that don't fit the variants above.
    /// Preserves backward compatibility: any `anyhow::Error` can be
    /// wrapped and propagated with `?`.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}
