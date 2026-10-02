//! `gt-app` — the composition root.
//!
//! The **only** place in the hexagonal architecture that knows every concrete technology: SQLite / tree-sitter /
//! the filesystem / the YAML knowledge base are assembled here into port implementations and injected into the
//! application layer. No other crate depends on a concrete technology.

pub mod config;
pub mod container;

pub use config::AppConfig;
pub use container::Container;
