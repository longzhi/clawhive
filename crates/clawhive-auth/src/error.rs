#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("profile not found: {0}")]
    ProfileNotFound(String),

    #[error("token refresh failed: {0}")]
    TokenRefreshFailed(String),

    #[error("configuration error: {0}")]
    ConfigError(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}
