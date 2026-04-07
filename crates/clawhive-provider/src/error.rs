/// Typed errors for the provider crate's public API.
///
/// Currently covers registry/config operations (`create_provider`,
/// `register_from_configs`, `ProviderRegistry::get`).
///
/// Used by all `LlmProvider` trait methods as well as registry/config
/// operations.
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

impl ProviderError {
    /// Whether this error is transient and the request should be retried.
    ///
    /// Rate limits, timeouts, and server errors (5xx) are retryable.
    /// Legacy `Other` errors containing `[retryable]` are also considered
    /// retryable for backward compatibility.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::RateLimited { .. } | Self::Timeout => true,
            Self::ApiError { status, .. } if (500..600).contains(status) => true,
            Self::Other(err) => err.to_string().contains("[retryable]"),
            _ => false,
        }
    }
}
