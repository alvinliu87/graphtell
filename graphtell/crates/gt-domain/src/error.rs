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

#[cfg(test)]
mod tests {
    use super::*;

    /// The adapter contract: `infra` always yields `Infrastructure` (never the concrete type) while preserving
    /// the `Display` text — this is what keeps a `rusqlite::Error` from leaking upward.
    #[test]
    fn infra_wraps_display_into_infrastructure_variant() {
        let e = DomainError::infra("disk full");
        match e {
            DomainError::Infrastructure(ref msg) => assert_eq!(msg, "disk full"),
            other => panic!("infra must produce the Infrastructure variant, got {other:?}"),
        }
        assert_eq!(e.to_string(), "infrastructure error: disk full");
    }

    /// Any `Display` type must be wrappable — the basis for `?` propagation inside adapters.
    #[test]
    fn infra_accepts_any_display_type() {
        #[derive(Debug)]
        struct MyErr;
        impl std::fmt::Display for MyErr {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "custom boom")
            }
        }
        let e = DomainError::infra(MyErr);
        assert!(matches!(e, DomainError::Infrastructure(_)));
        assert_eq!(e.to_string(), "infrastructure error: custom boom");
    }

    /// `serde_json::Error` must convert into the `Json` variant so adapters can `?`-propagate parse failures.
    #[test]
    fn from_serde_json_error_yields_json_variant() {
        let je: serde_json::Error = serde_json::from_str::<serde_json::Value>("{not valid").unwrap_err();
        let de: DomainError = je.into();
        assert!(matches!(de, DomainError::Json(_)), "a serde_json::Error must be converted to the Json variant");
    }

    /// Pin the `Display` format of the field-bearing variants so the `{file}: {reason}` / path wording is not
    /// silently edited (the kernel surfaces these strings to users).
    #[test]
    fn display_format_pins_key_variants() {
        assert_eq!(DomainError::NotFound("x".into()).to_string(), "resource not found: x");
        assert_eq!(
            DomainError::Parse { file: PathBuf::from("a/yaml"), reason: "bad".into() }.to_string(),
            "parse failed: a/yaml: bad"
        );
        assert_eq!(
            DomainError::PathEscape(PathBuf::from("../etc")).to_string(),
            "path out of bounds: ../etc"
        );
    }
}
