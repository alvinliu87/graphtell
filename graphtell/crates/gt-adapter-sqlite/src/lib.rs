//! `gt-adapter-sqlite` —— SQLite 持久化适配器。
//!
//! 实现 `gt-domain` 中定义的全部持久化端口（`ProjectReader` / `ProjectWriter` /
//! `GraphSink` / `GraphQuery` / `SymbolTableReader` / `DiagnosticSink`）。
//!
//! 图数据、工程元数据、权威符号表、诊断全部落在一张 SQLite 文件里，
//! 便于随工程一起分发与版本化。

pub mod schema;
pub mod store;

pub use store::SqliteStore;
