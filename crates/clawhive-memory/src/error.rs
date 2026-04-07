/// Typed error for `clawhive-memory` public API.
///
/// Applied to all `MemoryStore` public methods. Internal closures
/// (e.g. inside `spawn_blocking`) still use `anyhow::Result` and are
/// converted at the boundary via the `Other` variant.
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
