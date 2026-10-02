//! Domain error types.
//!
//! Following dependency inversion in SOLID: errors are defined in the kernel, and an adapter only converts its own
//! errors into [`DomainError::Infrastructure`], never leaking a concrete type such as `rusqlite::Error` upwards.

use std::path::PathBuf;

/// The unified result type of the domain layer.
pub type Result<T> = std::result::Result<T, DomainError>;

/// A domain error.
#[derive(Debug, thiserror::Error)]
pub enum DomainError {
    #[error("resource not found: {0}")]
    NotFound(String),

    #[error("invalid argument: {0}")]
    InvalidArgument(String),

    #[error("config / knowledge base corrupted: {0}")]
    InvalidKnowledge(String),

    #[error("parse failed: {file}: {reason}")]
    Parse { file: PathBuf, reason: String },

    #[error("path out of bounds: {0}")]
    PathEscape(PathBuf),

    #[error("conflict: {0}")]
    Conflict(String),

    #[error("unsupported capability: {0}")]
    Unsupported(String),

    /// An adapter boxes a concrete technical error and passes it through, keeping the `Display` text but leaking no type.
    #[error("infrastructure error: {0}")]
    Infrastructure(String),

    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

impl DomainError {
    /// For adapters: wrap any `Display` error as a domain error.
    pub fn infra<E: std::fmt::Display>(e: E) -> Self {
        DomainError::Infrastructure(e.to_string())
    }
}
