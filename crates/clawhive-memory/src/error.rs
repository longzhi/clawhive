/// Typed error for `clawhive-memory` public API.
///
/// Only applied to boundary functions where error classification adds value
/// (e.g. constructors). Internal helpers and most async methods continue to
/// use `anyhow::Result` as an escape hatch.
#[derive(Debug, thiserror::Error)]
pub enum MemoryError {
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("migration failed: {0}")]
    Migration(String),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}
