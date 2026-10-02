//! `gt-adapter-sqlite` — the SQLite persistence adapter.
//!
//! Implements every persistence port defined in `gt-domain` (`ProjectReader` / `ProjectWriter` /
//! `GraphSink` / `GraphQuery` / `SymbolTableReader` / `DiagnosticSink`).
//!
//! Graph data, project metadata, the authoritative symbol table and diagnostics all land in one SQLite
//! file, so they can be shipped and versioned together with the project.

pub mod schema;
pub mod store;

pub use store::SqliteStore;
