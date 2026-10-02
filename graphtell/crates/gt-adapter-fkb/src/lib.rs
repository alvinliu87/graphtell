//! `gt-adapter-fkb` — the framework knowledge base (FKB) adapter.
//!
//! Loads framework knowledge described in YAML into the domain model [`FrameworkKnowledge`]
//! and implements the [`KnowledgeProvider`] outbound port.
//!
//! The kernel knows no framework; adding framework support = adding one YAML file.

pub mod loader;

pub use loader::YamlKnowledgeBase;
